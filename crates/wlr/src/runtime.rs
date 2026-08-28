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

#[cfg(wlr_has_xwayland)]
use crate::XwaylandSurfaceId;
use crate::buffer::create_pixel_buffer;
use crate::decoration::{DecorationEntry, DecorationMode};
use crate::id::{SourceId, next_id};
use crate::layer::Layer;
use crate::scene::output::SceneOutputEntry;
use crate::scene::{
    LegacyId, NodeId, NodeKind, SceneBuffer, SceneBufferOptions, SceneNode, SceneOutput,
    SceneOutputId, SceneOutputStateOptions, SceneRect, SceneSurface, SceneTree, attach_node_id,
    find_node_id, timespec_of,
};
use crate::{
    AllocatorRef, Backend, Box2D, Buffer, BufferId, CursorShape, Display, Error, FBox, Interest,
    LayerSurfaceId, Output, OutputId, Popup, PopupId, PopupParent, RectId, Region, RendererRef,
    Result, ToplevelId, Transform, sys,
};
use crate::{ColorEncoding, ColorRange, FilterMode, NamedPrimaries, TransferFunction};

/// The Wayland **implicit pointer grab**: the surface that owns pointer
/// input for as long as a button is held, and the reference frame its
/// surface-local coordinates are measured from.
///
/// While this is set, `backend.rs`'s motion paths stop asking the scene
/// graph what the cursor is over and send every motion to `surface`
/// instead. The coordinates come from the same delta model sway's
/// `seatop_down` uses: whatever surface-local point the `enter` established
/// (`ref_sx`, `ref_sy`), displaced by however far the cursor has travelled
/// in layout coordinates since (`ref_lx`, `ref_ly`).
///
/// The model deliberately does **not** track the surface moving underneath
/// the held pointer: a window dragged, resized or reordered mid-press keeps
/// reporting coordinates relative to where it was when the button went
/// down. sway has the same limitation, and the alternative — re-deriving
/// the offset from the surface's live scene position every motion — would
/// make an interactive move (which moves the window *because* the pointer
/// moved) feed back on itself.
///
/// `surface` is a borrowed wlroots pointer, never owned. It is only ever
/// dereferenced after being compared equal to the seat's *current*
/// `focused_surface`, which wlroots itself nulls when the surface is
/// destroyed — so a grab left behind by a destroyed surface fails that
/// comparison and is dropped rather than followed.
#[derive(Clone, Copy)]
pub(crate) struct PointerGrab {
    /// The surface the press landed on and every subsequent event goes to.
    pub(crate) surface: *mut sys::wlr_surface,
    /// The cursor's layout position when the button went down.
    pub(crate) ref_lx: f64,
    /// The cursor's layout position when the button went down.
    pub(crate) ref_ly: f64,
    /// The surface-local coordinates the enter established at that point.
    pub(crate) ref_sx: f64,
    /// The surface-local coordinates the enter established at that point.
    pub(crate) ref_sy: f64,
}

impl PointerGrab {
    /// The surface-local coordinates to report for a cursor now at layout
    /// position `(x, y)`.
    ///
    /// Pure, and the whole of the grab's coordinate model — which is why it
    /// is a function rather than three lines inlined into two nearly
    /// identical motion handlers.
    pub(crate) fn surface_coords(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.ref_sx + (x - self.ref_lx),
            self.ref_sy + (y - self.ref_ly),
        )
    }
}

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

    /// Every scene node this runtime has issued a [`NodeId`] for.
    ///
    /// Written by `ensure_node_id` (which is where every id in the crate is
    /// minted) and purged by [`crate::scene::NodePurge`]'s `Drop`, which
    /// wlroots runs from the dying node's own addon set — including for every
    /// node of a recursive destroy cascade. That is why this table needs no
    /// parent tracking of its own, unlike `rects`/`buffers`, whose published
    /// id types predate the mechanism.
    pub(crate) nodes: RefCell<HashMap<NodeId, NodeEntry>>,

    /// How many [`Runtime::with_node`] borrows and [`Runtime::for_each_buffer`]
    /// walks are live on this thread.
    ///
    /// A [`SceneNode`](crate::SceneNode) handle holds a raw pointer, so a
    /// destroy reached from inside the closure that was handed the handle
    /// would leave it dangling for the rest of that call; a destroy reached
    /// from inside a `for_each_buffer` visitor is worse still, because wlroots
    /// is mid-`wl_list_for_each` over the very list the node sits in. Rather
    /// than documenting the hazard away, every call that can free or unlink a
    /// node refuses while this is non-zero. `Cell`, not `RefCell`: it is read
    /// from paths that must not be able to fail.
    pub(crate) node_borrows: std::cell::Cell<usize>,

    /// Every live RGBA pixel-buffer scene node. Same shape and same purge
    /// rules as `rects` — see [`BufferEntry`]'s own doc.
    pub(crate) buffers: RefCell<HashMap<BufferId, BufferEntry>>,

    /// The live run's scene-buffer observer, or `None` when no run is on the
    /// stack.
    ///
    /// Installed by `run_inner` for exactly the duration of one
    /// [`Backend::run_all`](crate::Backend::run_all) call and cleared by its
    /// guard on every exit path — see [`SceneObserver`] for why the indirection
    /// exists at all. `Cell`, not `RefCell`: it is read from
    /// [`Runtime::observe_scene_buffer`] and friends, which must not be able to
    /// fail on a borrow.
    pub(crate) scene_observer: std::cell::Cell<Option<SceneObserver>>,

    /// The scene outputs each observed buffer node was last reported to be
    /// displayed on.
    ///
    /// Snapshotted by `backend.rs`'s `on_scene_buffer_outputs_update` when the
    /// signal fires, because the C array it carries is valid only for that
    /// emission, and read back by [`Runtime::scene_buffer_active_outputs`].
    /// Purged when the node dies (`on_scene_buffer_node_destroy`) and when a
    /// consumer stops observing it.
    pub(crate) scene_buffer_outputs: RefCell<HashMap<NodeId, Vec<SceneOutputId>>>,

    /// Every scene output this runtime has issued a [`SceneOutputId`] for.
    ///
    /// Written by `record_scene_output` — reached from
    /// [`Runtime::init_output`], [`Runtime::add_scene_output`] and the lookup
    /// in [`Runtime::scene_output`] — and purged by each row's own destroy
    /// listener, which wlroots runs from inside `wlr_scene_output_destroy`.
    /// Deliberately **not** cleared per run, unlike `outputs`: a scene output
    /// belongs to the scene rather than to a `Backend::run_all` call, and its
    /// watch belongs to this runtime, so it stays truthful across runs. See
    /// `scene/output.rs`'s module doc for why the id needs a listener where
    /// every other id in this crate needs only an addon.
    pub(crate) scene_outputs: RefCell<HashMap<SceneOutputId, SceneOutputEntry>>,

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

    /// The screencopy (`zwlr_screencopy_manager_v1`) manager, once created —
    /// lets a client capture an output's rendered contents (screenshots,
    /// screen sharing). `Option`, same rationale as the other manager globals.
    pub(crate) screencopy_manager: RefCell<Option<NonNull<sys::wlr_screencopy_manager_v1>>>,

    /// The idle-notifier (`ext_idle_notifier_v1`) global, once created — lets
    /// a client (e.g. swayidle) be told when the seat has been idle for a
    /// timeout. `Option`, same rationale as the other manager globals.
    pub(crate) idle_notifier: RefCell<Option<NonNull<sys::wlr_idle_notifier_v1>>>,

    /// The idle-inhibit (`zwp_idle_inhibit_manager_v1`) manager, once
    /// created — lets a client (e.g. a video player) inhibit idling while a
    /// surface is visible. `Option`, same rationale as the other manager
    /// globals.
    pub(crate) idle_inhibit_manager: RefCell<Option<NonNull<sys::wlr_idle_inhibit_manager_v1>>>,

    /// The pointer-constraints (`zwp_pointer_constraints_v1`) manager, once
    /// created — lets a client confine or lock the pointer to a region of a
    /// surface. `Option`, same rationale as the other manager globals.
    pub(crate) pointer_constraints_manager:
        RefCell<Option<NonNull<sys::wlr_pointer_constraints_v1>>>,

    /// The relative-pointer (`zwp_relative_pointer_manager_v1`) manager, once
    /// created — lets a client receive unaccelerated relative pointer motion
    /// events, independent of absolute cursor position. `Option`, same
    /// rationale as the other manager globals.
    pub(crate) relative_pointer_manager:
        RefCell<Option<NonNull<sys::wlr_relative_pointer_manager_v1>>>,

    /// The cursor-shape (`wp_cursor_shape_manager_v1`) manager, once created —
    /// lets a client name the cursor image it wants instead of drawing its
    /// own. `Option`, same rationale as the other manager globals.
    pub(crate) cursor_shape_manager: RefCell<Option<NonNull<sys::wlr_cursor_shape_manager_v1>>>,

    /// The xdg-activation (`xdg_activation_v1`) manager, once created — lets a
    /// client request that one of its surfaces be given focus. `Option`, same
    /// rationale as the other manager globals.
    pub(crate) xdg_activation_manager: RefCell<Option<NonNull<sys::wlr_xdg_activation_v1>>>,

    /// The gamma-control (`zwlr_gamma_control_manager_v1`) manager, once
    /// created — lets a client (a night-light tool such as `wlsunset` or
    /// `gammastep`) set a per-output gamma ramp. `Option`, same rationale as
    /// the other manager globals. Wired into this runtime's scene the moment
    /// it is created — see [`Runtime::create_gamma_control_manager`] — so the
    /// scene applies every ramp and signals `failed`/`destroy` itself.
    pub(crate) gamma_control_manager: RefCell<Option<NonNull<sys::wlr_gamma_control_manager_v1>>>,

    /// The pointer constraint currently activated on the focused surface, or
    /// `None` when the pointer is unconstrained. `backend.rs`'s
    /// `on_pointer_constraint_destroy` clears this the moment the active
    /// constraint is destroyed (so enforcement stops); the activation policy —
    /// which constraint becomes active on a focus change — lands in a follow-up
    /// task. Init `None`.
    pub(crate) active_constraint: std::cell::Cell<Option<NonNull<sys::wlr_pointer_constraint_v1>>>,

    /// The implicit pointer grab in force, or `None` when no button is
    /// held. Written only by `backend.rs`'s button handler (set on the
    /// press that takes the button count from zero, cleared on the release
    /// that returns it to zero) and by the motion paths when the grab stops
    /// applying. `Cell`, like `active_constraint`: it is read from
    /// `extern "C"` frames that must not be able to fail on a borrow.
    pub(crate) pointer_grab: std::cell::Cell<Option<PointerGrab>>,

    /// The number of currently live `wlr_idle_inhibitor_v1` objects, tracked
    /// so [`Runtime::refresh_idle_inhibited`] knows whether to gate the idle
    /// notifier. `backend.rs`'s `on_new_idle_inhibitor`/
    /// `on_idle_inhibitor_destroy` are the only writers, incrementing and
    /// (saturating) decrementing respectively.
    pub(crate) idle_inhibitors: std::cell::Cell<usize>,

    /// The `ext_session_lock_manager_v1` global, once created — lets a locker
    /// (e.g. a lock screen) lock the session. `Option`, same rationale as the
    /// other manager globals: a consumer that never calls
    /// [`create_session_lock_manager`](Runtime::create_session_lock_manager)
    /// never advertises the global, and a second call would advertise a
    /// second one.
    pub(crate) session_lock_manager: RefCell<Option<NonNull<sys::wlr_session_lock_manager_v1>>>,

    /// The `zwlr_output_manager_v1` global, once created — lets a client
    /// (e.g. a display-settings app) enumerate output heads and request an
    /// atomic reconfiguration. `Option`, same rationale as the other manager
    /// globals: a consumer that never calls
    /// [`create_output_manager`](Runtime::create_output_manager) never
    /// advertises the global, and a second call would advertise a second one.
    pub(crate) output_manager: RefCell<Option<NonNull<sys::wlr_output_manager_v1>>>,

    /// The `wp_viewporter` global, once created — lets a client crop/scale
    /// its buffer via a viewport, applied by the scene at render time.
    /// `Option`, same rationale as the other manager globals.
    pub(crate) viewporter: RefCell<Option<NonNull<sys::wlr_viewporter>>>,

    /// The `wp_single_pixel_buffer_manager_v1` global, once created — lets a
    /// client create a cheap solid-colour buffer without a shm/dmabuf pool.
    /// `Option`, same rationale as the other manager globals.
    pub(crate) single_pixel_buffer_manager:
        RefCell<Option<NonNull<sys::wlr_single_pixel_buffer_manager_v1>>>,

    /// The `wp_content_type_manager_v1` global, once created — lets a client
    /// tag a surface's content type (video, game, ...) as hint metadata.
    /// `Option`, same rationale as the other manager globals.
    pub(crate) content_type_manager: RefCell<Option<NonNull<sys::wlr_content_type_manager_v1>>>,

    /// The `zxdg_output_manager_v1` global, once created — advertises
    /// read-only per-output logical geometry to clients (many panels and
    /// toolkits read this). `Option`, same rationale as the other manager
    /// globals.
    pub(crate) xdg_output_manager: RefCell<Option<NonNull<sys::wlr_xdg_output_manager_v1>>>,

    /// The `wp_fractional_scale_manager_v1` global, once created — lets a
    /// client learn its preferred fractional output scale so it can render a
    /// sharp buffer. `Option`, same rationale as the other manager globals.
    pub(crate) fractional_scale_manager:
        RefCell<Option<NonNull<sys::wlr_fractional_scale_manager_v1>>>,

    /// The `wp_presentation` global, once created — lets a client request
    /// presentation feedback (when its buffer was actually presented, and at
    /// what refresh). `Option`, same rationale as the other manager globals.
    pub(crate) presentation: RefCell<Option<NonNull<sys::wlr_presentation>>>,

    /// The `wlr_compositor` this runtime created in
    /// [`Runtime::init_graphics`]. Stored — unlike the renderer/allocator, kept
    /// inside [`Graphics`] — because [`Runtime::create_xwayland`] needs it after
    /// the fact and it is the one wlroots object a manager-create call outside
    /// `init_graphics` reaches for. `None` until graphics is initialised.
    #[cfg(wlr_has_xwayland)]
    pub(crate) compositor: RefCell<Option<NonNull<sys::wlr_compositor>>>,

    /// The `wlr_xwayland` manager, once created — advertises the X server and
    /// bridges X11 windows into the compositor. `Option`, same rationale as the
    /// other manager globals: a consumer that never calls
    /// [`Runtime::create_xwayland`] never starts Xwayland, and it is
    /// display/runtime-owned (no `Drop`, torn down with the display, matching
    /// the session-lock and idle managers).
    #[cfg(wlr_has_xwayland)]
    pub(crate) xwayland: RefCell<Option<NonNull<sys::wlr_xwayland>>>,

    /// Every live Xwayland surface (X11 window): the role object and its scene
    /// tree once associated. Mirrors [`toplevels`](RuntimeInner::toplevels) in
    /// shape and lifetime — populated by `backend.rs`'s `on_new_xwayland_surface`,
    /// the tree filled in on `associate` and cleared on `unassociate`, and the
    /// whole entry removed by `on_xwayland_surface_destroy` before wlroots frees
    /// the surface.
    ///
    /// Keyed by [`XwaylandSurfaceId`] rather than by a raw pointer for the same
    /// reason every by-id table here is: a raw pointer can alias a freed-then-
    /// reused object across a destroy, an id cannot.
    #[cfg(wlr_has_xwayland)]
    pub(crate) xwayland_surfaces: RefCell<HashMap<XwaylandSurfaceId, XwaylandSurfaceEntry>>,

    /// Whether the session is currently locked. **The security bit.** Set true
    /// the instant a locker takes a lock (`backend.rs`'s
    /// `on_new_session_lock`) and cleared **only** on a genuine unlock
    /// (`on_session_unlock`) — a locker that dies without unlocking leaves
    /// this true (`on_session_lock_destroy`), which is exactly what keeps the
    /// screen locked when the lock process crashes. Read by
    /// [`Runtime::is_session_locked`] and by every focus/hit-test entry point
    /// in this crate to refuse input to normal clients while locked. Init
    /// `false`.
    pub(crate) session_locked: std::cell::Cell<bool>,

    /// The active `wlr_session_lock_v1`, while one is held. `None` before any
    /// lock and between a lock dying and the next taking over — note
    /// [`session_locked`](RuntimeInner::session_locked) can be `true` while
    /// this is `None` (a locker died without unlocking; the session stays
    /// locked until a new locker unlocks). Set by `on_new_session_lock`,
    /// cleared by `on_session_lock_destroy`.
    pub(crate) session_lock: RefCell<Option<NonNull<sys::wlr_session_lock_v1>>>,

    /// Whether the active lock asked to unlock before it was destroyed. Set by
    /// `on_session_unlock`, reset to `false` when a fresh lock is taken
    /// (`on_new_session_lock`). This is the flag `on_session_lock_destroy`
    /// reads to tell a genuine unlock (complete the teardown) from a locker
    /// dying (**stay locked** — the security invariant). Init `false`.
    pub(crate) session_unlock_requested: std::cell::Cell<bool>,

    /// Whether `wlr_session_lock_v1_send_locked` has already been sent for the
    /// active lock. The protocol's `locked` event is sent exactly once per
    /// lock, only after every output is covered by a committed lock surface;
    /// this guards that "once". Reset when a fresh lock is taken and when a
    /// lock is destroyed. Init `false`.
    pub(crate) session_locked_sent: std::cell::Cell<bool>,

    /// Every current lock surface's scene tree, keyed by the `*mut wlr_output`
    /// (as `usize`) it covers, alongside the underlying `wlr_surface` so the
    /// "all outputs covered" check can read its `mapped` flag. Populated by
    /// `on_session_lock_new_surface` (which creates the tree in
    /// [`Band::Lock`]); an entry is removed by `on_session_lock_surface_destroy`
    /// **before** wlroots frees the tree (the subsurface tree is destroyed
    /// with its surface), and the whole map is cleared on unlock and on lock
    /// destroy. The stored tree pointer is never dereferenced through this map
    /// — only inserted and dropped — so an entry momentarily outliving its
    /// tree could never be a use-after-free; the per-surface destroy listener
    /// removes it regardless.
    pub(crate) lock_surface_trees: RefCell<HashMap<usize, LockSurfaceRender>>,

    /// The opaque black fill covering every output while the session is
    /// locked, parented into [`Band::Lock`] **beneath** every lock surface
    /// (created before any surface arrives, so it stays at the bottom of the
    /// band). This is what makes the spec's "blank Lock band covering every
    /// output" actually opaque: a live locker's surface renders over it, but
    /// any gap — a dead locker's freed surface, an uncovered or hotplugged
    /// output — shows solid black rather than the normal toplevels beneath.
    /// Created by [`Runtime::install_lock_fill`] when the session locks,
    /// removed by [`Runtime::remove_lock_fill`] on a genuine unlock. `None`
    /// when unlocked. Reused (repositioned) rather than duplicated across a
    /// crashed-locker takeover, which calls `begin_session_lock` a second
    /// time while this fill is still live.
    pub(crate) session_lock_fill: std::cell::Cell<Option<RectId>>,

    /// Every live toplevel: the role object, its scene tree, and the surface
    /// its id addon lives on.
    pub(crate) toplevels: RefCell<HashMap<ToplevelId, ToplevelEntry>>,

    /// Every live popup: the role object, its scene subtree, and the parent it
    /// was announced under.
    ///
    /// Purged per-popup by [`Runtime::forget_popup`] (from
    /// `on_popup_destroy`, before wlroots frees the popup) and wholesale by
    /// [`Runtime::clear_popups`] when the `run_all` call that announced them
    /// returns — the identical two-level discipline `toplevels` has, and for
    /// the identical reason.
    pub(crate) popups: RefCell<HashMap<PopupId, PopupEntry>>,

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
    /// The shape a `cursor-shape-v1` client named through
    /// [`Runtime::set_cursor_shape`], and which
    /// [`Runtime::ensure_cursor_image`] must therefore *not* stomp back to
    /// `left_ptr` on the next pointer motion. `None` means "no shape is
    /// named" — the default `left_ptr` image — which is both the initial
    /// state and what a pointer-focus change resets to (see `backend.rs`'s
    /// `on_pointer_focus_change`).
    pub(crate) named_cursor: std::cell::Cell<Option<CursorShape>>,
    /// What was last handed to `wlr_cursor_set_xcursor`, in the same
    /// encoding as `named_cursor` (`Some(None)` = the default `left_ptr`).
    /// The outer `None` means nothing has been applied yet, so the very
    /// first call always reaches wlroots. Purely an FFI short-circuit:
    /// wlroots' own `wlr_cursor_set_xcursor` already early-returns on an
    /// unchanged manager+name pair, so this only saves the call itself —
    /// but it is also what makes "the image was not stomped" observable to
    /// this crate's own tests.
    pub(crate) applied_cursor: std::cell::Cell<Option<Option<CursorShape>>>,

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

    /// Test-only: whether [`Runtime::enable_test_touch`] has been called.
    /// There is no touch input device and never will be one recorded here —
    /// unlike `keyboards`/`pointers`, this is not counting anything real, it
    /// is a standing request that `backend.rs`'s `update_seat_capabilities`
    /// OR the touch bit into every capability recompute it does from now on,
    /// so the bit survives a keyboard or pointer later being hot-plugged or
    /// unplugged instead of being clobbered by the next recompute. Default
    /// `false`, so a consumer that never calls `enable_test_touch` gets
    /// byte-identical capability behaviour to a build without this field.
    pub(crate) test_touch_enabled: std::cell::Cell<bool>,

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

    /// The drag icon's scene tree, while a drag with a visible icon is in
    /// progress — the tree `wlr_scene_drag_icon_create` returns from
    /// `backend.rs`'s `on_start_drag`. `None` when no drag is active, or the
    /// active drag carries no icon (`on_start_drag` returns early on a null
    /// icon without ever populating this).
    ///
    /// Contrary to what an earlier revision of this doc claimed, upstream
    /// `wlr_scene_drag_icon` does **not** self-track the pointer/touch
    /// position — its only reposition listener fires on the icon surface's
    /// own buffer-commit deltas, never on cursor motion (verified against
    /// wlroots 0.20.2's own C source). So this crate's own motion handlers
    /// write to the tree's position on every motion — see
    /// [`Runtime::reposition_drag_icon`] and its callers — and this cell is
    /// what makes that possible: it exists both so those handlers have
    /// something to reposition and so [`Runtime::drag_icon_position`] can
    /// read the result back for observability (chiefly tests asserting the
    /// icon renders and follows the input).
    ///
    /// Cleared back to `None` by the per-drag-icon destroy listener
    /// `on_start_drag` registers on `(*(*drag).icon).events.destroy` — see
    /// that function's own doc for why the *icon's* destroy, not the drag's,
    /// is the listener that has to run here. That listener is the only thing
    /// standing between this cell and a dangling pointer: the scene tree is
    /// owned by wlroots and freed when the icon is destroyed, so a stale
    /// `Some` here past that point would be a use-after-free waiting to
    /// happen on the next read. See [`Runtime::drag_icon_position`]'s SAFETY
    /// comment for the full argument.
    pub(crate) drag_icon_tree: RefCell<Option<NonNull<sys::wlr_scene_tree>>>,

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

/// One live Xwayland surface (X11 window) as this crate tracks it.
///
/// `tree` is `None` until the surface associates a `wlr_surface` — an X11
/// window exists before it has content — and returns to `None` on unassociate.
/// Like [`crate::runtime::LockSurfaceRender`]'s tree, it is created by
/// `wlr_scene_subsurface_tree_create` and **never destroyed by this crate**:
/// wlroots owns the subsurface tree and frees it together with the `wlr_surface`
/// it was built from, so the crate only ever stores and drops the pointer.
#[cfg(wlr_has_xwayland)]
pub(crate) struct XwaylandSurfaceEntry {
    pub(crate) raw: NonNull<sys::wlr_xwayland_surface>,
    /// The scene tree the surface renders through while associated. Read by
    /// [`Runtime::set_xwayland_surface_position`]/`_visible`/`raise_xwayland_surface`
    /// to move, hide or restack what is drawn, but never freed here — wlroots
    /// owns the subsurface tree and frees it together with the `wlr_surface`.
    pub(crate) tree: std::cell::Cell<Option<NonNull<sys::wlr_scene_tree>>>,
}

/// One current lock surface as this crate tracks it: the scene tree it renders
/// through (a child of [`Band::Lock`], created by
/// `wlr_scene_subsurface_tree_create`) and the underlying `wlr_surface` whose
/// `mapped` flag the "all outputs covered" check reads. See
/// [`RuntimeInner::lock_surface_trees`] for the lifetime rules — the `tree`
/// here is never dereferenced through the map, only stored and dropped.
#[derive(Clone, Copy)]
pub(crate) struct LockSurfaceRender {
    /// The scene tree the lock surface renders through. Stored to name the
    /// per-output lock-surface tree (the crate's model of what covers each
    /// output) and to hold the handle for the surface's whole lifetime, but
    /// **never dereferenced** and never destroyed by this crate: wlroots owns
    /// the subsurface tree and frees it with the surface, so this crate only
    /// ever inserts and drops the pointer. Hence `dead_code`-allowed — its
    /// value is its presence in the map, not a read.
    #[allow(dead_code)]
    pub(crate) tree: NonNull<sys::wlr_scene_tree>,
    pub(crate) surface: *mut sys::wlr_surface,
}

/// A live layer surface: the role object, the scene tree
/// `wlr_scene_layer_surface_v1_create` created for it, and any configure
/// size waiting for this surface's initial commit to become safe to send.
///
/// One live popup as this crate tracks it.
///
/// Not `Copy`, unlike [`ToplevelEntry`]: `configured` is a `Cell`, and `Cell` is
/// never `Copy` regardless of what it holds. Every accessor that needs `raw` or
/// `tree` outside a held borrow copies just that field out —
/// [`Runtime::popup_raw`]/[`Runtime::popup_tree`] — the same narrowing
/// `LayerSurfaceEntry`'s accessors do for the identical reason.
///
/// `tree` is the subtree `wlr_scene_xdg_surface_create` built **under the
/// parent's tree**, which is what makes a popup stack with its parent for free
/// and what makes `leaf_surface_at` resolve clicks on it — including the
/// session-lock isolation gate — without a line of new code. It is stored and
/// dropped, **never destroyed**: wlroots frees a tree's children recursively
/// with the tree, so destroying a child of a dying parent is a double free (see
/// [`Runtime::forget_toplevel`]'s own comment).
pub(crate) struct PopupEntry {
    pub(crate) raw: NonNull<sys::wlr_xdg_popup>,
    // wired up by a later task in this part (`popup_tree`'s only reader)
    #[allow(dead_code)]
    pub(crate) tree: NonNull<sys::wlr_scene_tree>,
    /// The parent recorded at announcement time. A layer popup's
    /// `(*popup).parent` is NULL then, so this is the only place the answer
    /// exists — see [`PopupParent`]'s own doc.
    pub(crate) parent: PopupParent,
    /// Whether [`Runtime::configure_popup`] has ever sent a configure for this
    /// popup. Read by nothing in P1's own logic; it is the flag a compositor's
    /// reactive-reposition pass needs to tell "never placed" from "placed and
    /// due a re-place", and it is cheaper to record here, at the one site that
    /// knows, than to reconstruct downstream.
    // wired up by a later task in this part (`mark_popup_configured`'s only
    // reader, itself unused until `configure_popup` lands)
    #[allow(dead_code)]
    pub(crate) configured: std::cell::Cell<bool>,
}

/// Not `Copy`, unlike [`ToplevelEntry`]: `staged_configure` is a `Cell`, and
/// `Cell` is never `Copy` regardless of what it holds. Every accessor that
/// needs to read `raw`/`scene_tree` outside a held borrow copies just that
/// field out — `layer_surface_ptr`/`layer_surface_scene_ptr` — the same
/// narrowing [`crate::decoration::DecorationEntry`]'s own accessors do for
/// the identical reason.
pub(crate) struct LayerSurfaceEntry {
    pub(crate) raw: NonNull<sys::wlr_layer_surface_v1>,
    pub(crate) scene_tree: NonNull<sys::wlr_scene_tree>,

    /// The `wlr_scene_layer_surface_v1` helper `scene_tree` belongs to.
    ///
    /// Kept alongside the tree rather than recovered from it: the tree is the
    /// helper's first field, so a `container_of` cast would work today, but it
    /// would be an unchecked layout assumption about a struct this crate does
    /// not own for the sake of eight bytes. wlroots frees this together with
    /// the layer surface, so it is live for exactly as long as the entry is.
    /// [`Runtime::configure_scene_layer_surface`] is what needs it.
    pub(crate) scene: NonNull<sys::wlr_scene_layer_surface_v1>,

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
/// later parents by band) can live in — the same six stacking bands
/// `Graphics` creates, plus [`Band::Toplevel`] for the band every
/// toplevel's own tree lives in.
///
/// Deliberately **not** [`Layer`]: `Layer` is the public four-variant
/// protocol vocabulary a layer-shell client speaks
/// (`Background`/`Bottom`/`Top`/`Overlay`, `layer.rs`'s own type), and
/// reusing it here would either strand `Band::Toplevel` outside that
/// vocabulary or force a fifth variant onto a type whose four variants are
/// already frozen as of 0.20.x's layer-shell surface. `Band` is a new,
/// separate enum instead, covering exactly the six bands
/// [`Runtime::add_rect_in_band`] can target — [`Band::Lock`] included.
///
/// That Lock is targetable is deliberate and **load-bearing, not an
/// oversight to close**. The opaque fill this crate drops over every output
/// when a locker dies without unlocking is itself an
/// `add_rect_in_band(Band::Lock, ..)` call, so a guard rejecting that band
/// would break the stay-locked-on-death path — turning a locked session into
/// an exposed desktop, which is the one outcome the lock state machine
/// exists to prevent.
///
/// It is not a way past the lock either: this API is compositor-facing and
/// no client can reach it. The boundary the lock defends is
/// compositor-vs-client, and a compositor drawing into its own lock band is
/// its own business.
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
    /// Above everything except `Lock` — `Graphics::overlay_band`.
    Overlay,
    /// Above `Overlay` — the session-lock band. Only used while the session is
    /// locked; lock surfaces render here, covering all normal content and
    /// layer-shell. `Graphics::lock_band`.
    Lock,
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

/// The live run's ability to watch a scene buffer node for this runtime.
///
/// The five signals a `wlr_scene_buffer` emits go to a **handler**, so the
/// listeners that carry them must belong to a
/// [`Backend::run_all`](crate::Backend::run_all) call — that is where the
/// `&mut S` and the delivery function live. But the *nodes* belong to the
/// runtime, and so does the by-id API a consumer uses to ask for one to be
/// watched. This is the join: `run_all` plants its own erased `Session`
/// pointer and three functions instantiated at its own `S`, and
/// [`Runtime::observe_scene_buffer`] calls through them.
///
/// `Copy`, so reading it out of the `Cell` leaves the `Cell` populated — the
/// hook must survive the call that used it.
#[derive(Clone, Copy)]
pub(crate) struct SceneObserver {
    /// An erased `*const Session<'_, S>`, valid for exactly as long as this
    /// value is installed. `run_inner`'s guard is what makes that true.
    pub(crate) session: *const (),
    /// Links the six listeners, or does nothing if they are already linked.
    ///
    /// # Safety
    ///
    /// The caller must pass `session` verbatim and a live `wlr_scene_buffer`
    /// whose node carries the given [`NodeId`].
    pub(crate) watch: unsafe fn(*const (), NodeId, *mut sys::wlr_scene_buffer),
    /// Unlinks them again. `# Safety`: `session` must be passed verbatim.
    pub(crate) unwatch: unsafe fn(*const (), NodeId),
    /// Whether they are linked. `# Safety`: as for `unwatch`.
    pub(crate) is_watching: unsafe fn(*const (), NodeId) -> bool,
}

/// Where a tracked scene node came from, and therefore what may be done to it.
///
/// See [`crate::scene`]'s module docs for the argument; this is that
/// three-way split as a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeOrigin {
    /// Created through this crate's own node or rect/buffer API. Fully
    /// mutable.
    Owned,
    /// The scene root or one of the six bands. Readable, and
    /// [`Runtime::set_node_enabled`] works; nothing may destroy, restack or
    /// reparent one.
    Protected,
    /// A node wlroots or another part of this crate owns, which got an id
    /// because something observed it. Read-only through the node API.
    Foreign,
}

/// A scene node this runtime has an id for.
///
/// Not `Copy`: `alive` is an [`Rc`] shared with the node's own addon payload,
/// which clears it from wlroots' destroy hook. That flag, not this row, is the
/// authority on whether `raw` still points at anything — the row's removal is
/// best-effort (see [`crate::scene::NodePurge`]'s `Drop`).
#[derive(Clone)]
pub(crate) struct NodeEntry {
    pub(crate) raw: NonNull<sys::wlr_scene_node>,
    pub(crate) origin: NodeOrigin,
    pub(crate) alive: Rc<std::cell::Cell<bool>>,
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

    /// The six stacking bands, direct children of `scene.tree` (the scene
    /// root) in exactly this order — bottom to top:
    /// `background_band`, `bottom_band`, `toplevel_band`, `top_band`,
    /// `overlay_band`, `lock_band`. Created once, together, right after `scene` itself
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
    /// (`wl_list_insert(parent->children.prev, ...)`), creating these six
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
    /// at the end of the root's own children list, above all six bands),
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
    pub(crate) lock_band: NonNull<sys::wlr_scene_tree>,
}

impl Graphics {
    /// The scene tree `band` names. Total — every [`Band`] variant maps to
    /// exactly one of the six fields above.
    pub(crate) fn band_tree(&self, band: Band) -> NonNull<sys::wlr_scene_tree> {
        match band {
            Band::Background => self.background_band,
            Band::Bottom => self.bottom_band,
            Band::Toplevel => self.toplevel_band,
            Band::Top => self.top_band,
            Band::Overlay => self.overlay_band,
            Band::Lock => self.lock_band,
        }
    }
}

/// Raises `RuntimeInner::node_borrows` for the life of a scene-node borrow or
/// of a [`Runtime::for_each_buffer`] walk.
///
/// A [`SceneNode`] handle is a raw pointer with a lifetime; the lifetime stops
/// it escaping the closure, but nothing in the type system stops that closure
/// from calling [`Runtime::destroy_node`] on the very node it was handed. This
/// guard is what does: while it is held, every call that can free a node
/// refuses. `Drop` lowers the count on every path, including an unwind, so a
/// panicking closure cannot leave a runtime permanently unable to destroy
/// anything.
///
/// [`Runtime::for_each_buffer`] raises the same guard for a sharper reason than
/// a dangling handle: wlroots walks each tree's child list with
/// `wl_list_for_each`, not the `_safe` form, so it reads the current node's
/// `link.next` *after* the visitor returns. Freeing or unlinking that node from
/// inside the visitor is a use-after-free inside wlroots' own recursion, not
/// merely a stale Rust handle.
struct NodeBorrowGuard<'a> {
    inner: &'a RuntimeInner,
}

impl<'a> NodeBorrowGuard<'a> {
    fn enter(inner: &'a RuntimeInner) -> NodeBorrowGuard<'a> {
        // Saturating, not wrapping: a count that wrapped to zero would silently
        // re-permit destroys mid-borrow. It cannot happen — each live borrow is
        // a stack frame — but the failure mode is bad enough to spell out.
        inner
            .node_borrows
            .set(inner.node_borrows.get().saturating_add(1));
        NodeBorrowGuard { inner }
    }
}

impl Drop for NodeBorrowGuard<'_> {
    fn drop(&mut self) {
        self.inner
            .node_borrows
            .set(self.inner.node_borrows.get().saturating_sub(1));
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
                nodes: RefCell::new(HashMap::new()),
                node_borrows: std::cell::Cell::new(0),
                buffers: RefCell::new(HashMap::new()),
                scene_observer: std::cell::Cell::new(None),
                scene_buffer_outputs: RefCell::new(HashMap::new()),
                scene_outputs: RefCell::new(HashMap::new()),
                live_sources: RefCell::new(HashMap::new()),
                pending_close: RefCell::new(Vec::new()),
                xdg_shell: RefCell::new(None),
                xdg_decoration_manager: RefCell::new(None),
                primary_selection_manager: RefCell::new(None),
                data_control_manager: RefCell::new(None),
                virtual_keyboard_manager: RefCell::new(None),
                virtual_pointer_manager: RefCell::new(None),
                screencopy_manager: RefCell::new(None),
                pointer_constraints_manager: RefCell::new(None),
                relative_pointer_manager: RefCell::new(None),
                cursor_shape_manager: RefCell::new(None),
                xdg_activation_manager: RefCell::new(None),
                gamma_control_manager: RefCell::new(None),
                active_constraint: std::cell::Cell::new(None),
                pointer_grab: std::cell::Cell::new(None),
                idle_notifier: RefCell::new(None),
                idle_inhibit_manager: RefCell::new(None),
                idle_inhibitors: std::cell::Cell::new(0),
                session_lock_manager: RefCell::new(None),
                output_manager: RefCell::new(None),
                viewporter: RefCell::new(None),
                single_pixel_buffer_manager: RefCell::new(None),
                content_type_manager: RefCell::new(None),
                xdg_output_manager: RefCell::new(None),
                fractional_scale_manager: RefCell::new(None),
                presentation: RefCell::new(None),
                #[cfg(wlr_has_xwayland)]
                compositor: RefCell::new(None),
                #[cfg(wlr_has_xwayland)]
                xwayland: RefCell::new(None),
                #[cfg(wlr_has_xwayland)]
                xwayland_surfaces: RefCell::new(HashMap::new()),
                session_locked: std::cell::Cell::new(false),
                session_lock: RefCell::new(None),
                session_unlock_requested: std::cell::Cell::new(false),
                session_locked_sent: std::cell::Cell::new(false),
                lock_surface_trees: RefCell::new(HashMap::new()),
                session_lock_fill: std::cell::Cell::new(None),
                toplevels: RefCell::new(HashMap::new()),
                popups: RefCell::new(HashMap::new()),
                decorations: RefCell::new(HashMap::new()),
                layer_shell: RefCell::new(None),
                layer_surfaces: RefCell::new(HashMap::new()),
                tree_to_toplevel: RefCell::new(HashMap::new()),
                seat: RefCell::new(None),
                cursor: RefCell::new(None),
                xcursor: RefCell::new(None),
                cursor_image_loaded: std::cell::Cell::new(false),
                named_cursor: std::cell::Cell::new(None),
                applied_cursor: std::cell::Cell::new(None),
                keyboards: RefCell::new(Vec::new()),
                pointers: RefCell::new(Vec::new()),
                test_touch_enabled: std::cell::Cell::new(false),
                outputs: RefCell::new(HashMap::new()),
                drag_icon_tree: RefCell::new(None),
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
        //
        // `in_delivery()`, not `in_handler()`. The latter is also raised by
        // the scene borrow guards, which cannot be holding an fd borrow — that
        // borrow exists only for the duration of an `fd_ready` call. Asking
        // the broad question made `rt.with_node(id, |_| rt.remove_fd(src))`
        // with no run on the stack queue the fd against `drain_pending_closes`,
        // which only a run performs, so it was never closed at all.
        if crate::dispatch::in_delivery() {
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
    ///
    /// # Call this before the run, not from inside it
    ///
    /// [`Backend::run_all`](crate::Backend::run_all) links its listeners once,
    /// on entry, and the renderer's `lost` signal is among them — so it is
    /// linked only if a renderer already exists at that moment. Initialising
    /// graphics later, from inside
    /// [`OutputHandler::new_output`](crate::OutputHandler::new_output) for
    /// instance, produces a working renderer that nothing is watching, and
    /// [`OutputHandler::renderer_lost`](crate::OutputHandler::renderer_lost)
    /// is then never delivered for the rest of that run.
    pub fn init_graphics(&self, display: &Display, backend: &Backend<'_>) -> Result<()> {
        if self.inner.graphics.borrow().is_some() {
            return Err(Error::Operation("Runtime::init_graphics called twice"));
        }
        // Frees `scene` (and every band already attached to it) if this
        // function returns early via `?` anywhere after the scene is
        // created. Nothing else does: `Graphics` has no `Drop`, and none of
        // the fallible steps below (the six bands, the output layout, the
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

            // The six stacking bands, created in bottom-to-top order right
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
            let lock_band = sys::wlr_scene_tree_create(&raw mut (*scene.as_ptr()).tree);
            let lock_band = NonNull::new(lock_band)
                .ok_or(Error::Create("wlr_scene_tree_create (lock band)"))?;

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
            // Kept for `create_xwayland`, which needs the compositor after the
            // fact; every other consumer of it lives inside this block.
            #[cfg(wlr_has_xwayland)]
            {
                *self.inner.compositor.borrow_mut() = NonNull::new(compositor);
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
                lock_band,
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
        let scene = graphics.scene;
        let bands = [
            graphics.background_band,
            graphics.bottom_band,
            graphics.toplevel_band,
            graphics.top_band,
            graphics.overlay_band,
            graphics.lock_band,
        ];
        *self.inner.graphics.borrow_mut() = Some(graphics);
        // Ids for the seven nodes a consumer can name but must not restructure:
        // the scene root and the six bands.
        // Attached *after* the fallible section, so `SceneGuard`'s rollback
        // never has to unwind a half-populated node table: on that path the
        // scene is destroyed before any id exists.
        //
        // SAFETY: the scene root and the six band trees were created above
        // and are live; none carries a node payload yet (this runs once —
        // `init_graphics` refuses a second call). `Protected` is what stops
        // `destroy_node`/`reparent_node` reaching them.
        unsafe {
            self.record_node(
                &raw mut (*scene.as_ptr()).tree.node,
                NodeOrigin::Protected,
                None,
            );
            for band in bands {
                self.record_node(&raw mut (*band.as_ptr()).node, NodeOrigin::Protected, None);
            }
        }
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

        // `wlr_scene_output_create` may be called at most once per (scene,
        // output) pair: it plants an addon keyed by that pair, and a second
        // call reaches `wlr_addon_init`'s
        // `assert(0 && "Can't have two addons of the same type with the same
        // owner")`, which Arch compiles in — so the process dies rather than
        // returning an error.
        //
        // `add_scene_output` has probed for this since it was written; this
        // path never did, and it is the one every doc and example tells a
        // compositor to call from `new_output`. A second `new_output` for one
        // output (a re-plug racing a slow first init, or a consumer that also
        // calls it from its own setup) took the process down.
        //
        // Reported rather than silently accepted: a consumer that init'd the
        // same output twice has a bookkeeping bug, and hiding it behind `Ok`
        // would leave them wondering why the second output never renders.
        // SAFETY: the handle's lifetime guarantees the output is live, and the
        // scene is this runtime's own.
        if !unsafe { sys::wlr_scene_get_scene_output(scene.as_ptr(), output.as_ptr()) }.is_null() {
            return Err(Error::Operation("Runtime::init_output called twice"));
        }

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

            // Since 0.20.19: give the scene output an id, and start watching it
            // for destruction, so a consumer can reach it by id
            // (`Runtime::scene_output`) and so a stale one misses cleanly. Done
            // last, after every fallible step: on an early return above there
            // is no scene output to watch.
            //
            // SAFETY: `scene_output` is the one wlroots just created in this
            // runtime's own scene, so it is live and nothing watches it yet.
            self.record_scene_output(scene_output);
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
    ///
    /// [`Error::Operation`] also while any mapping opened by
    /// [`Buffer::begin_data_ptr_access`](crate::Buffer::begin_data_ptr_access)
    /// is live on this thread. A commit textures whatever buffers the scene
    /// graph holds, which this crate cannot enumerate to check one by one, and
    /// texturing a shared-memory buffer opens wlroots' own data-pointer
    /// bracket on it — whose entry `assert(!buffer->accessing_data_ptr)` would
    /// abort the process if the scene happened to hold the mapped buffer.
    /// Refusing every commit while a mapping is open is the conservative
    /// reading of a question that cannot be asked precisely; drop the guard
    /// before committing.
    pub fn commit_output(&self, output: &Output<'_>) -> Result<()> {
        if crate::buffer::any_data_ptr_access_open() {
            return Err(Error::Operation("Runtime::commit_output"));
        }
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
            let mut now = timespec_of(now_dur);
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

    /// Ask wlroots to fire [`OutputHandler::frame`](crate::OutputHandler::frame)
    /// for the output this [`OutputId`] names, resolving the id back to its
    /// `*mut wlr_output` the same way
    /// [`output_layout_box`](Runtime::output_layout_box) and
    /// [`set_output_position`](Runtime::set_output_position) do.
    ///
    /// This is the id-keyed sibling of
    /// [`Output::schedule_frame`](crate::Output::schedule_frame): identical
    /// effect (`wlr_output_schedule_frame`), for the callers that hold only an
    /// [`OutputId`] and no live [`Output`](crate::Output) handle — most
    /// notably an output re-enabled through
    /// [`OutputHandler::output_configuration_applied`](crate::OutputHandler::output_configuration_applied),
    /// which is handed heads and ids but no `Output`, and still needs the
    /// one-time kick so a freshly re-enabled output that draws nothing gets its
    /// first `frame` callback and commit.
    ///
    /// Returns `None` on an unknown or stale id (see
    /// [`output_layout_box`](Runtime::output_layout_box)'s own doc for that
    /// rule); `Some(())` once the frame has been scheduled. Infallible past the
    /// id lookup, because `wlr_output_schedule_frame` returns nothing.
    pub fn schedule_frame(&self, id: OutputId) -> Option<()> {
        let raw = self.output_ptr(id)?;
        // SAFETY: a present `outputs` entry names an output still linked into
        // that table (removed synchronously by `forget_output` before wlroots
        // frees it), so `raw` is live. `wlr_output_schedule_frame` only marks
        // the output for a frame; it does not dispatch or re-enter this crate.
        unsafe { sys::wlr_output_schedule_frame(raw.as_ptr()) };
        Some(())
    }

    /// Schedule a frame on every live output, returning how many were kicked.
    ///
    /// The [`schedule_frame`](Runtime::schedule_frame) sibling for the case
    /// where the caller has no particular output in mind and wants the frame
    /// clock advanced everywhere — used to flush pending `wl_surface.frame`
    /// callbacks that would otherwise starve on an undamaged, headless output
    /// (an xwayland window renders its first buffer only once its initial
    /// bufferless commit's frame callback is answered).
    pub fn schedule_frame_all(&self) -> usize {
        let outputs = self.inner.outputs.borrow();
        for raw in outputs.values() {
            // SAFETY: as `schedule_frame` — every value in `outputs` names a
            // live wlr_output (removed synchronously by `forget_output` before
            // wlroots frees it); the call only marks the output for a frame.
            unsafe { sys::wlr_output_schedule_frame(raw.as_ptr()) };
        }
        outputs.len()
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
        // Refused while a node borrow or a scene walk is live.
        //
        // The borrow gate was added to every call that *unlinks* a node and to
        // none that *inserts* one, which left the more dangerous half open:
        // wlroots walks with `wl_list_for_each`, not the `_safe` variant, so
        // its cursor holds a raw `next` pointer. Unlinking mid-walk was
        // refused; appending rewires the tail the cursor is about to reach,
        // and `for_each_buffer` then never terminates —
        // `rt.for_each_buffer(t, |..| { rt.create_scene_buffer(t, None); })`
        // allocates without bound, from entirely safe code.
        //
        // Refused for any live borrow rather than only for the tree the walk
        // is standing in, because which tree the cursor has reached is not
        // knowable from here — and a rule that holds only sometimes is the
        // one that gets relied on.
        if self.scene_is_being_walked() {
            return Err(Error::Reentrant("scene insertion during a walk"));
        }
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
        // SAFETY: `raw` is the node wlroots just created, so it is live and
        // carries no payload of this kind yet. The payload is what drops the
        // row above when a cascade frees the node — see `NodePurge`.
        unsafe {
            self.record_node(
                &raw mut (*raw.as_ptr()).node,
                NodeOrigin::Owned,
                Some(LegacyId::Rect(id)),
            );
        }
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
        // Refused while a node borrow or a scene walk is live.
        //
        // The borrow gate was added to every call that *unlinks* a node and to
        // none that *inserts* one, which left the more dangerous half open:
        // wlroots walks with `wl_list_for_each`, not the `_safe` variant, so
        // its cursor holds a raw `next` pointer. Unlinking mid-walk was
        // refused; appending rewires the tail the cursor is about to reach,
        // and `for_each_buffer` then never terminates —
        // `rt.for_each_buffer(t, |..| { rt.create_scene_buffer(t, None); })`
        // allocates without bound, from entirely safe code.
        //
        // Refused for any live borrow rather than only for the tree the walk
        // is standing in, because which tree the cursor has reached is not
        // knowable from here — and a rule that holds only sometimes is the
        // one that gets relied on.
        if self.scene_is_being_walked() {
            return None;
        }
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
        // SAFETY: as in `add_rect` — a freshly created, live node.
        unsafe {
            self.record_node(
                &raw mut (*raw.as_ptr()).node,
                NodeOrigin::Owned,
                Some(LegacyId::Rect(id)),
            );
        }
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
        // Refused while a node borrow or a scene walk is live.
        //
        // The borrow gate was added to every call that *unlinks* a node and to
        // none that *inserts* one, which left the more dangerous half open:
        // wlroots walks with `wl_list_for_each`, not the `_safe` variant, so
        // its cursor holds a raw `next` pointer. Unlinking mid-walk was
        // refused; appending rewires the tail the cursor is about to reach,
        // and `for_each_buffer` then never terminates —
        // `rt.for_each_buffer(t, |..| { rt.create_scene_buffer(t, None); })`
        // allocates without bound, from entirely safe code.
        //
        // Refused for any live borrow rather than only for the tree the walk
        // is standing in, because which tree the cursor has reached is not
        // knowable from here — and a rule that holds only sometimes is the
        // one that gets relied on.
        if self.scene_is_being_walked() {
            return Err(Error::Reentrant("scene insertion during a walk"));
        }
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
        // SAFETY: `tree` names one of the six band trees `init_graphics`
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
        // SAFETY: as in `add_rect` — a freshly created, live node.
        unsafe {
            self.record_node(
                &raw mut (*raw.as_ptr()).node,
                NodeOrigin::Owned,
                Some(LegacyId::Rect(id)),
            );
        }
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
    ///
    /// Also `None`, having removed nothing, while a
    /// [`with_node`](Runtime::with_node) borrow or a
    /// [`for_each_buffer`](Runtime::for_each_buffer) walk is live — that
    /// borrow's handle would otherwise dangle for the rest of the closure, and
    /// that walk would read a freed `wl_list` link. Only code inside such a
    /// closure can observe this, so it is additive to the published 0.20.5
    /// behaviour rather than a change to it.
    pub fn remove_rect(&self, rect: RectId) -> Option<()> {
        if self.scene_is_being_walked() {
            return None;
        }
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

    /// Raise a rect above its siblings in whatever tree it is parented into —
    /// the node counterpart of [`raise_toplevel`](Runtime::raise_toplevel), for
    /// a band-parented decoration rect that has to ride its window's z-order.
    /// `None` if this runtime never issued `rect`, and (also `None`, raising
    /// nothing) while a scene walk is live, for the same
    /// [`raise_toplevel`](Runtime::raise_toplevel) reason.
    pub fn raise_rect(&self, rect: RectId) -> Option<()> {
        if self.scene_is_being_walked() {
            return None;
        }
        let raw = self.rect_ptr(rect)?;
        // SAFETY: as for `set_rect_position` — a resolvable id names a node
        // wlroots has not destroyed.
        unsafe { sys::wlr_scene_node_raise_to_top(&raw mut (*raw.as_ptr()).node) };
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
        // Refused while a scene borrow or buffer walk is live. wlroots
        // iterates with `wl_list_for_each`, not the `_safe` variant, so
        // unlinking a node and reinserting it elsewhere mid-walk leaves the
        // iteration reading `link.next` from where the node used to be — it
        // silently stops early rather than crashing, which is worse. The
        // destroy calls refuse for this reason; the restacks unlink just as
        // thoroughly and did not, until this was added alongside them.
        if self.scene_is_being_walked() {
            return None;
        }
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
        // Refused while a node borrow or a scene walk is live.
        //
        // The borrow gate was added to every call that *unlinks* a node and to
        // none that *inserts* one, which left the more dangerous half open:
        // wlroots walks with `wl_list_for_each`, not the `_safe` variant, so
        // its cursor holds a raw `next` pointer. Unlinking mid-walk was
        // refused; appending rewires the tail the cursor is about to reach,
        // and `for_each_buffer` then never terminates —
        // `rt.for_each_buffer(t, |..| { rt.create_scene_buffer(t, None); })`
        // allocates without bound, from entirely safe code.
        //
        // Refused for any live borrow rather than only for the tree the walk
        // is standing in, because which tree the cursor has reached is not
        // knowable from here — and a rule that holds only sometimes is the
        // one that gets relied on.
        if self.scene_is_being_walked() {
            return Err(Error::Reentrant("scene insertion during a walk"));
        }
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
        // SAFETY: as in `add_rect` — a freshly created, live node.
        unsafe {
            self.record_node(
                &raw mut (*node.as_ptr()).node,
                NodeOrigin::Owned,
                Some(LegacyId::Buffer(id)),
            );
        }
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
        // Refused while a node borrow or a scene walk is live.
        //
        // The borrow gate was added to every call that *unlinks* a node and to
        // none that *inserts* one, which left the more dangerous half open:
        // wlroots walks with `wl_list_for_each`, not the `_safe` variant, so
        // its cursor holds a raw `next` pointer. Unlinking mid-walk was
        // refused; appending rewires the tail the cursor is about to reach,
        // and `for_each_buffer` then never terminates —
        // `rt.for_each_buffer(t, |..| { rt.create_scene_buffer(t, None); })`
        // allocates without bound, from entirely safe code.
        //
        // Refused for any live borrow rather than only for the tree the walk
        // is standing in, because which tree the cursor has reached is not
        // knowable from here — and a rule that holds only sometimes is the
        // one that gets relied on.
        if self.scene_is_being_walked() {
            return None;
        }
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
        // SAFETY: as in `add_rect` — a freshly created, live node.
        unsafe {
            self.record_node(
                &raw mut (*node.as_ptr()).node,
                NodeOrigin::Owned,
                Some(LegacyId::Buffer(id)),
            );
        }
        Some(id)
    }

    /// Add a pixel buffer parented into `band`'s own scene tree, the buffer
    /// counterpart of [`add_rect_in_band`](Runtime::add_rect_in_band). Like a
    /// band rect — and unlike [`add_buffer_in_toplevel`](Runtime::add_buffer_in_toplevel) —
    /// it is a sibling of every toplevel/xwayland/layer-surface tree in that
    /// band, so it stacks *with* them, and it is **never** purged by a toplevel
    /// dying: only [`remove_buffer`](Runtime::remove_buffer) or tearing down the
    /// scene destroys it (its `parent` is recorded as `None`, exactly as a root
    /// buffer's is, since neither is a descendant of any single toplevel).
    ///
    /// This is what lets a compositor paint server-side decorations for an X11
    /// window, whose scene node lives directly in [`Band::Toplevel`] rather than
    /// in a per-toplevel tree.
    ///
    /// Coordinates given to [`set_buffer_position`](Runtime::set_buffer_position)
    /// afterward are relative to the band tree's own origin, which for every
    /// band is the scene root's origin.
    ///
    /// `None` on any of [`add_buffer`](Runtime::add_buffer)'s error conditions
    /// (wrong pixel length, a non-positive or overflow-prone dimension, no
    /// graphics yet, or wlroots refusing the node), and refused (also `None`)
    /// while a scene walk is live, for the identical reason
    /// [`add_buffer_in_toplevel`](Runtime::add_buffer_in_toplevel) is.
    pub fn add_buffer_in_band(
        &self,
        band: Band,
        width: i32,
        height: i32,
        rgba: &[u8],
    ) -> Option<BufferId> {
        if self.scene_is_being_walked() {
            return None;
        }
        if !crate::buffer::validate_pixels(width, height, rgba.len()) {
            return None;
        }
        let band_tree = self.band_ptr(band)?;
        let buf = create_pixel_buffer(width, height, rgba);
        // SAFETY: the band tree is created by `init_graphics` and lives for the
        // whole scene's life; the `wlr_buffer_drop` pairing is identical to
        // `add_buffer`'s — see that method's own comment and `buffer.rs`.
        let node = unsafe { sys::wlr_scene_buffer_create(band_tree.as_ptr(), buf) };
        // SAFETY: as for `add_buffer`.
        unsafe { sys::wlr_buffer_drop(buf) };
        let node = NonNull::new(node)?;
        let id = BufferId(next_id());
        self.inner
            .buffers
            .borrow_mut()
            .insert(id, BufferEntry { node, parent: None });
        // SAFETY: as in `add_rect` — a freshly created, live node.
        unsafe {
            self.record_node(
                &raw mut (*node.as_ptr()).node,
                NodeOrigin::Owned,
                Some(LegacyId::Buffer(id)),
            );
        }
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

    /// Raise a buffer node above its siblings — the buffer counterpart of
    /// [`raise_rect`](Runtime::raise_rect), for a band-parented decoration
    /// glyph/title buffer that has to ride its window's z-order. `None` if this
    /// runtime never issued `buffer`, and (also `None`) while a scene walk is
    /// live.
    pub fn raise_buffer(&self, buffer: BufferId) -> Option<()> {
        if self.scene_is_being_walked() {
            return None;
        }
        let node = self.buffer_ptr(buffer)?;
        // SAFETY: as for `set_buffer_position`.
        unsafe { sys::wlr_scene_node_raise_to_top(&raw mut (*node.as_ptr()).node) };
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
        // Refused while a scene borrow or buffer walk is live. wlroots
        // iterates with `wl_list_for_each`, not the `_safe` variant, so
        // unlinking a node and reinserting it elsewhere mid-walk leaves the
        // iteration reading `link.next` from where the node used to be — it
        // silently stops early rather than crashing, which is worse. The
        // destroy calls refuse for this reason; the restacks unlink just as
        // thoroughly and did not, until this was added alongside them.
        if self.scene_is_being_walked() {
            return None;
        }
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
    ///
    /// Also `None`, having removed nothing, while a
    /// [`with_node`](Runtime::with_node) borrow or a
    /// [`for_each_buffer`](Runtime::for_each_buffer) walk is live — see
    /// [`remove_rect`](Runtime::remove_rect)'s own note.
    pub fn remove_buffer(&self, buffer: BufferId) -> Option<()> {
        if self.scene_is_being_walked() {
            return None;
        }
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

    // ---------------------------------------------------------------------
    // Scene nodes (0.20.19)
    //
    // Every id in this section is addon-backed: the node carries a payload
    // whose `Drop` wlroots runs when it frees the node, including for every
    // node of a recursive destroy cascade. That is what makes a stale
    // `NodeId` miss cleanly rather than name freed memory, and it is why none
    // of these methods needs the parent tracking `RectEntry` still carries
    // for the frozen 0.20.1 rect API.
    // ---------------------------------------------------------------------

    /// Attach a fresh [`NodeId`] to `node` and record it.
    ///
    /// `None` only for a null `node`, which is how wlroots reports a failed
    /// constructor.
    ///
    /// # Safety
    ///
    /// A non-null `node` must point at a live `wlr_scene_node` that does not
    /// already carry one of this crate's node payloads — `wlr_addon_init`
    /// `assert()`s on a duplicate and aborts (see `addon.rs`).
    unsafe fn record_node(
        &self,
        node: *mut sys::wlr_scene_node,
        origin: NodeOrigin,
        legacy: Option<LegacyId>,
    ) -> Option<NodeId> {
        let raw = NonNull::new(node)?;
        // SAFETY: forwarded from this function's own contract.
        let (id, alive) = unsafe { attach_node_id(node, &self.inner, legacy) };
        self.inner
            .nodes
            .borrow_mut()
            .insert(id, NodeEntry { raw, origin, alive });
        Some(id)
    }

    /// The id attached to `node`, minting one if it has none.
    ///
    /// Idempotent, and that is a requirement rather than an optimisation:
    /// [`node_at`](Runtime::node_at) and the children walk reach nodes that
    /// may already be tracked, and a second `wlr_addon_init` under the same
    /// `(owner, impl)` pair aborts the process.
    ///
    /// # Safety
    ///
    /// `node` must point at a live `wlr_scene_node`.
    unsafe fn ensure_node_id(
        &self,
        node: *mut sys::wlr_scene_node,
        origin: NodeOrigin,
    ) -> Option<NodeId> {
        // SAFETY: forwarded.
        if let Some(id) = unsafe { find_node_id(node) } {
            return Some(id);
        }
        // SAFETY: forwarded; the lookup above established that `node` carries
        // no payload of this kind yet.
        unsafe { self.record_node(node, origin, None) }
    }

    /// The row `id` names, or `None` if the node is gone.
    ///
    /// The liveness flag, not the row's presence, is the authority: the row
    /// removal in the destroy hook is best-effort, the flag is not (see
    /// `NodePurge`'s own `Drop`).
    fn node_entry(&self, id: NodeId) -> Option<NodeEntry> {
        let entry = self.inner.nodes.borrow().get(&id).cloned()?;
        entry.alive.get().then_some(entry)
    }

    /// `id`'s node, whatever it belongs to. Reads only; see the
    /// [`scene`](crate::scene) module docs for what that excludes.
    fn node_ptr(&self, id: NodeId) -> Option<NonNull<sys::wlr_scene_node>> {
        Some(self.node_entry(id)?.raw)
    }

    /// `id`'s node, if this crate created it on a consumer's instruction.
    ///
    /// The gate on every *structural* change. Restacking or destroying a node
    /// wlroots owns — a toplevel's tree, a layer surface's tree, a drag icon —
    /// would desynchronise the placement bookkeeping
    /// [`raise_toplevel`](Runtime::raise_toplevel) and
    /// `reparent_layer_surface_if_changed` maintain, and destroying a band or
    /// the scene root would invalidate half this crate's tables at once.
    fn owned_node_ptr(&self, id: NodeId) -> Option<NonNull<sys::wlr_scene_node>> {
        let entry = self.node_entry(id)?;
        if self.is_locked_lock_band_descendant(entry.raw) {
            return None;
        }
        (entry.origin == NodeOrigin::Owned).then_some(entry.raw)
    }

    /// Whether the scene graph must not be restructured right now.
    ///
    /// Two independent reasons, and every gate needs both — which is why they
    /// are asked in one place. This test was hand-copied into twenty-one
    /// methods, each with its own comment, and the copies were where the gaps
    /// lived: the ones that unlink a node had it, the ones that insert one did
    /// not, and neither kind knew about foreign frames at all.
    ///
    /// A **live handle borrow** (`node_borrows`): a closure is holding a
    /// `SceneNode<'_>` or a sibling, and destroying the node it names leaves it
    /// dangling.
    ///
    /// A **foreign frame** ([`crate::dispatch::in_foreign_frame`]): wlroots is
    /// running our code from inside one of its own calls — a `for_each_buffer`
    /// visitor, a scene-output commit, a timeline waiter — and its list walks
    /// use `wl_list_for_each`, not the `_safe` form. Its cursor holds a raw
    /// `next`, so unlinking is a use-after-free inside its recursion and
    /// inserting silently rewires the tail it is about to reach.
    fn scene_is_being_walked(&self) -> bool {
        self.inner.node_borrows.get() != 0 || crate::dispatch::in_foreign_frame()
    }

    /// Whether `raw` is the [`Band::Lock`] band or anything beneath it, while
    /// a lock is actually held.
    ///
    /// The band itself was never the interesting target. What hides the
    /// desktop is the opaque black fill *inside* it, and that fill is created
    /// through `add_rect_in_band` like any other rect, so it is
    /// `NodeOrigin::Owned` and fully mutable — and it is reachable from safe
    /// code in two public calls: `band_node(Band::Lock)` then `node_children`.
    /// Hiding it, destroying it, or walking it off-screen uncovers a live
    /// desktop while `is_session_locked()` still reports `true`, which is
    /// verbatim what refusing on the band alone was supposed to prevent. Lock
    /// surfaces live under the same band and want the same protection.
    ///
    /// Walks wlroots' own `parent` chain rather than this crate's tables,
    /// because that chain is what actually decides what draws over what — a
    /// node reparented into the band by any route inherits the rule.
    ///
    /// Guards only the `NodeId` API, deliberately. The crate installs and
    /// repositions the fill through `RectId` (`set_rect_size`,
    /// `set_rect_position`, `remove_rect`), which resolves through `rect_ptr`
    /// on a separate path, so `install_lock_fill`'s takeover branch keeps
    /// working while locked — and a consumer cannot reach that path, because
    /// the fill's `RectId` is never handed out.
    fn is_locked_lock_band_descendant(&self, raw: NonNull<sys::wlr_scene_node>) -> bool {
        if !self.is_session_locked() {
            return false;
        }
        let Some(band) = self.band_ptr(Band::Lock) else {
            return false;
        };
        // SAFETY: `band_ptr` returns this runtime's own scene tree, live for as
        // long as its graphics are; taking the address of its embedded `node`
        // reads nothing.
        let band_node: *mut sys::wlr_scene_node = unsafe { &raw mut (*band.as_ptr()).node };
        let mut cursor: *mut sys::wlr_scene_node = raw.as_ptr();
        loop {
            if std::ptr::eq(cursor, band_node) {
                return true;
            }
            // SAFETY: `cursor` is a live node — the caller resolved it, and each
            // step moves to its parent tree, which outlives its children.
            let parent = unsafe { (*cursor).parent };
            if parent.is_null() {
                return false;
            }
            // SAFETY: `parent` is a live `wlr_scene_tree` whose `node` is
            // embedded by value.
            cursor = unsafe { &raw mut (*parent).node };
        }
    }

    /// `id`'s node, if moving or restyling it is allowed — nodes this crate
    /// created, and only those.
    ///
    /// Excludes `Protected` because a band tree's origin is `(0, 0)` by
    /// construction and [`add_rect_in_band`](Runtime::add_rect_in_band)'s
    /// documented coordinate space depends on it staying there.
    ///
    /// Excludes `Foreign` because those nodes belong to wlroots or to another
    /// part of this crate — a toplevel's tree, a layer surface's tree, a
    /// client's surface node — and this crate keeps placement bookkeeping for
    /// them that a direct move would silently invalidate. `scene`'s module
    /// doc has always promised that "every mutator on a protected or foreign
    /// node returns `None`"; this used to test only for `Protected`, so the
    /// promise held for half the nodes it named and
    /// [`set_node_position`](Runtime::set_node_position) could walk a
    /// toplevel's own tree out from under
    /// [`set_toplevel_position`](Runtime::set_toplevel_position) and
    /// [`raise_toplevel`](Runtime::raise_toplevel). Restack those through the
    /// toplevel and layer APIs, which update the bookkeeping as they go.
    fn movable_node_ptr(&self, id: NodeId) -> Option<NonNull<sys::wlr_scene_node>> {
        let entry = self.node_entry(id)?;
        if self.is_locked_lock_band_descendant(entry.raw) {
            return None;
        }
        (entry.origin == NodeOrigin::Owned).then_some(entry.raw)
    }

    /// `id`'s node as a tree, or `None` if it is not one.
    fn node_tree_ptr(&self, id: NodeId) -> Option<NonNull<sys::wlr_scene_tree>> {
        let raw = self.node_ptr(id)?;
        // SAFETY: a resolvable id names a live node.
        if NodeKind::from_raw(unsafe { (*raw.as_ptr()).type_ }) != Some(NodeKind::Tree) {
            return None;
        }
        // SAFETY: the tag says this node is a tree, which is
        // `wlr_scene_tree_from_node`'s whole precondition.
        NonNull::new(unsafe { sys::wlr_scene_tree_from_node(raw.as_ptr()) })
    }

    /// `id`'s node as a rect, if it is one and may be restyled.
    fn movable_rect_ptr(&self, id: NodeId) -> Option<NonNull<sys::wlr_scene_rect>> {
        let raw = self.movable_node_ptr(id)?;
        // SAFETY: a resolvable id names a live node.
        if NodeKind::from_raw(unsafe { (*raw.as_ptr()).type_ }) != Some(NodeKind::Rect) {
            return None;
        }
        // SAFETY: the tag says this node is a rect.
        NonNull::new(unsafe { sys::wlr_scene_rect_from_node(raw.as_ptr()) })
    }

    /// `id`'s node as a buffer node, if it is one and may be restyled.
    fn movable_scene_buffer_ptr(&self, id: NodeId) -> Option<NonNull<sys::wlr_scene_buffer>> {
        let raw = self.movable_node_ptr(id)?;
        // SAFETY: a resolvable id names a live node.
        if NodeKind::from_raw(unsafe { (*raw.as_ptr()).type_ }) != Some(NodeKind::Buffer) {
            return None;
        }
        // SAFETY: the tag says this node is a buffer node.
        NonNull::new(unsafe { sys::wlr_scene_buffer_from_node(raw.as_ptr()) })
    }

    /// The scene root's id. `None` before
    /// [`init_graphics`](Runtime::init_graphics) has run.
    ///
    /// Readable, and [`set_node_enabled`](Runtime::set_node_enabled) works on
    /// it; nothing may destroy, restack or reparent it.
    pub fn scene_root_node(&self) -> Option<NodeId> {
        let scene = self.scene_ptr()?;
        // SAFETY: the scene is this runtime's own and lives for the process.
        unsafe { find_node_id(&raw const (*scene.as_ptr()).tree.node) }
    }

    /// `band`'s tree, as a node id. `None` before
    /// [`init_graphics`](Runtime::init_graphics) has run.
    ///
    /// This is the parent to hand
    /// [`create_tree_under`](Runtime::create_tree_under) or
    /// [`create_rect`](Runtime::create_rect) when the new node should stack
    /// *with* a band rather than float above every one of them — see
    /// [`add_rect`](Runtime::add_rect)'s own doc for that trap.
    pub fn band_node(&self, band: Band) -> Option<NodeId> {
        let tree = self.band_ptr(band)?;
        // SAFETY: a band tree is this runtime's own and lives for the process.
        unsafe { find_node_id(&raw const (*tree.as_ptr()).node) }
    }

    /// The node id of a rect made with the 0.20.1 API.
    ///
    /// [`RectId`]'s representation is frozen and is not a node id, but the
    /// node underneath one is tracked like any other — this is the bridge, so
    /// a rect from [`add_rect`](Runtime::add_rect) can be restacked and
    /// reparented through the node API. `None` for an unknown or stale
    /// `rect`.
    pub fn rect_node(&self, rect: RectId) -> Option<NodeId> {
        let raw = self.rect_ptr(rect)?;
        // SAFETY: `rect_ptr` resolving means the node has not been destroyed —
        // the same argument `set_rect_position`'s own comment makes.
        unsafe { find_node_id(&raw const (*raw.as_ptr()).node) }
    }

    /// The node id of a pixel buffer made with the 0.20.3 API.
    ///
    /// The [`BufferId`] half of [`rect_node`](Runtime::rect_node); see that
    /// method's doc. `None` for an unknown or stale `buffer`.
    pub fn buffer_node(&self, buffer: BufferId) -> Option<NodeId> {
        let raw = self.buffer_ptr(buffer)?;
        // SAFETY: `buffer_ptr` resolving means the node has not been destroyed.
        unsafe { find_node_id(&raw const (*raw.as_ptr()).node) }
    }

    /// Create an empty scene tree as a direct child of `band`.
    ///
    /// The new tree is that band's topmost child until something says
    /// otherwise: wlroots appends every new node at the end of its parent's
    /// child list.
    ///
    /// `None` before [`init_graphics`](Runtime::init_graphics) has run, or if
    /// wlroots could not create the tree.
    pub fn create_tree_in_band(&self, band: Band) -> Option<NodeId> {
        // Refused while a node borrow or a scene walk is live.
        //
        // The borrow gate was added to every call that *unlinks* a node and to
        // none that *inserts* one, which left the more dangerous half open:
        // wlroots walks with `wl_list_for_each`, not the `_safe` variant, so
        // its cursor holds a raw `next` pointer. Unlinking mid-walk was
        // refused; appending rewires the tail the cursor is about to reach,
        // and `for_each_buffer` then never terminates —
        // `rt.for_each_buffer(t, |..| { rt.create_scene_buffer(t, None); })`
        // allocates without bound, from entirely safe code.
        //
        // Refused for any live borrow rather than only for the tree the walk
        // is standing in, because which tree the cursor has reached is not
        // knowable from here — and a rule that holds only sometimes is the
        // one that gets relied on.
        if self.scene_is_being_walked() {
            return None;
        }
        let parent = self.band_ptr(band)?;
        // SAFETY: `parent` is one of the six band trees `init_graphics`
        // created and this runtime owns; it outlives the call.
        let tree = unsafe { sys::wlr_scene_tree_create(parent.as_ptr()) };
        let tree = NonNull::new(tree)?;
        // SAFETY: `tree` is the tree wlroots just created, so nothing has had
        // the chance to attach a payload of this kind to it.
        unsafe { self.record_node(&raw mut (*tree.as_ptr()).node, NodeOrigin::Owned, None) }
    }

    /// Create an empty scene tree as a direct child of `parent`.
    ///
    /// `None` if `parent` is unknown, stale, or not a tree — a rect and a
    /// buffer node have no children, and asking is not an error.
    pub fn create_tree_under(&self, parent: NodeId) -> Option<NodeId> {
        // Refused while a node borrow or a scene walk is live.
        //
        // The borrow gate was added to every call that *unlinks* a node and to
        // none that *inserts* one, which left the more dangerous half open:
        // wlroots walks with `wl_list_for_each`, not the `_safe` variant, so
        // its cursor holds a raw `next` pointer. Unlinking mid-walk was
        // refused; appending rewires the tail the cursor is about to reach,
        // and `for_each_buffer` then never terminates —
        // `rt.for_each_buffer(t, |..| { rt.create_scene_buffer(t, None); })`
        // allocates without bound, from entirely safe code.
        //
        // Refused for any live borrow rather than only for the tree the walk
        // is standing in, because which tree the cursor has reached is not
        // knowable from here — and a rule that holds only sometimes is the
        // one that gets relied on.
        if self.scene_is_being_walked() {
            return None;
        }
        let parent = self.node_tree_ptr(parent)?;
        // SAFETY: a resolvable id names a live tree.
        let tree = unsafe { sys::wlr_scene_tree_create(parent.as_ptr()) };
        let tree = NonNull::new(tree)?;
        // SAFETY: as in `create_tree_in_band`.
        unsafe { self.record_node(&raw mut (*tree.as_ptr()).node, NodeOrigin::Owned, None) }
    }

    /// Destroy `node` and, recursively, every descendant.
    ///
    /// wlroots has no "orphan the children" mode; the cascade is the only
    /// behaviour. Every descendant's id misses cleanly afterwards — each node
    /// carries the addon payload whose destructor drops its row, and wlroots
    /// runs those for the whole cascade.
    ///
    /// `None`, having destroyed nothing, when:
    ///
    /// * `node` is unknown or already destroyed — a double destroy misses
    ///   cleanly rather than double-freeing;
    /// * `node` is the scene root or one of the six bands;
    /// * `node` is one wlroots owns (a toplevel's tree, a layer surface's
    ///   tree, a drag icon) — tear those down through the object that owns
    ///   them;
    /// * a [`with_node`](Runtime::with_node) borrow is live, which would leave
    ///   the handle that borrow produced dangling;
    /// * a [`for_each_buffer`](Runtime::for_each_buffer) walk is live, which
    ///   would leave wlroots' own `wl_list_for_each` reading a freed link.
    pub fn destroy_node(&self, node: NodeId) -> Option<()> {
        if self.scene_is_being_walked() {
            return None;
        }
        let raw = self.owned_node_ptr(node)?;
        // SAFETY: a resolvable id names a node wlroots has not freed — its
        // addon payload would have cleared the liveness flag otherwise — and
        // this crate created it, so nothing else destroys it in parallel. The
        // rows for this node and every descendant are dropped from the inside,
        // by those payloads' own destructors, during this call.
        unsafe { sys::wlr_scene_node_destroy(raw.as_ptr()) };
        Some(())
    }

    /// Show or hide `node` and, implicitly, everything under it.
    ///
    /// Disabling does not change any descendant's own flag; wlroots composes
    /// them at draw time, which is why re-enabling restores exactly what the
    /// subtree looked like. `None` for an unknown or stale id.
    ///
    /// Also `None` for disabling [`Band::Lock`] while the session is locked.
    /// That band is what a lock is *made of* — the opaque fill and every lock
    /// surface live in it — so hiding it uncovers the desktop underneath while
    /// [`is_session_locked`](Runtime::is_session_locked) still reports `true`
    /// and every other part of the crate still behaves as though locked. The
    /// screen shows a session the compositor believes is locked. Input stays
    /// isolated, so this was only ever visual, which is precisely the whole
    /// point of a lock screen.
    ///
    /// Re-*enabling* it is always allowed, and so is disabling it when no lock
    /// is held — an unlocked Lock band is empty, and refusing there would make
    /// the band uniquely unmanageable for no benefit.
    pub fn set_node_enabled(&self, node: NodeId, enabled: bool) -> Option<()> {
        let raw = self.node_ptr(node)?;
        if !enabled && self.is_locked_lock_band_descendant(raw) {
            return None;
        }
        // SAFETY: a resolvable id names a live node.
        unsafe { sys::wlr_scene_node_set_enabled(raw.as_ptr(), enabled) };
        Some(())
    }

    /// Move `node` to `(x, y)` **relative to its parent**.
    ///
    /// `None` for an unknown or stale id, and for the scene root and the five
    /// bands: their origin is `(0, 0)` by construction and
    /// [`add_rect_in_band`](Runtime::add_rect_in_band)'s documented coordinate
    /// space depends on it.
    pub fn set_node_position(&self, node: NodeId, x: i32, y: i32) -> Option<()> {
        let raw = self.movable_node_ptr(node)?;
        // SAFETY: a resolvable id names a live node.
        unsafe { sys::wlr_scene_node_set_position(raw.as_ptr(), x, y) };
        Some(())
    }

    /// Put `node` directly above `sibling` in their shared parent.
    ///
    /// `None` — with nothing moved — when either id is unknown or stale, when
    /// they name the same node, when they do not share a parent, or when
    /// `node` is not one this crate created for the consumer.
    /// `wlr_scene_node_place_above` **asserts** the first three, and Arch
    /// ships wlroots with assertions enabled, so an unchecked call would be a
    /// process abort rather than a recoverable error.
    ///
    /// `sibling` may be any node, including a toplevel's own tree: putting a
    /// rect directly above one particular window inside the toplevel band is
    /// exactly what this is for.
    pub fn place_node_above(&self, node: NodeId, sibling: NodeId) -> Option<()> {
        self.place_node(node, sibling, true)
    }

    /// Put `node` directly below `sibling` in their shared parent. The mirror
    /// of [`place_node_above`](Runtime::place_node_above), with the same
    /// refusals for the same reasons.
    pub fn place_node_below(&self, node: NodeId, sibling: NodeId) -> Option<()> {
        self.place_node(node, sibling, false)
    }

    /// The shared body of [`place_node_above`](Runtime::place_node_above) and
    /// [`place_node_below`](Runtime::place_node_below): one copy of the three
    /// assert-avoiding checks, so they cannot drift apart.
    fn place_node(&self, node: NodeId, sibling: NodeId, above: bool) -> Option<()> {
        // Refused while a scene borrow or buffer walk is live. wlroots
        // iterates with `wl_list_for_each`, not the `_safe` variant, so
        // unlinking a node and reinserting it elsewhere mid-walk leaves the
        // iteration reading `link.next` from where the node used to be — it
        // silently stops early rather than crashing, which is worse. The
        // destroy calls refuse for this reason; the restacks unlink just as
        // thoroughly and did not, until this was added alongside them.
        if self.scene_is_being_walked() {
            return None;
        }
        if node == sibling {
            return None;
        }
        let raw = self.owned_node_ptr(node)?;
        let other = self.node_ptr(sibling)?;
        if raw == other {
            return None;
        }
        // SAFETY: both ids resolve, so both name live nodes; this reads their
        // `parent` fields and nothing else.
        let shared = unsafe { (*raw.as_ptr()).parent == (*other.as_ptr()).parent };
        if !shared {
            return None;
        }
        // SAFETY: the checks above are exactly `wlr_scene_node_place_*`'s own
        // asserts — distinct nodes, same parent — and both nodes are live.
        unsafe {
            if above {
                sys::wlr_scene_node_place_above(raw.as_ptr(), other.as_ptr());
            } else {
                sys::wlr_scene_node_place_below(raw.as_ptr(), other.as_ptr());
            }
        }
        Some(())
    }

    /// Make `node` the topmost of its siblings.
    ///
    /// Siblings only: this cannot lift a node out of the band it lives in,
    /// which is what makes this crate's band ordering permanent (see
    /// `Graphics`' own doc). `None` for an unknown or stale id, or one this
    /// crate did not create for the consumer.
    pub fn raise_node_to_top(&self, node: NodeId) -> Option<()> {
        // Refused while a scene borrow or buffer walk is live. wlroots
        // iterates with `wl_list_for_each`, not the `_safe` variant, so
        // unlinking a node and reinserting it elsewhere mid-walk leaves the
        // iteration reading `link.next` from where the node used to be — it
        // silently stops early rather than crashing, which is worse. The
        // destroy calls refuse for this reason; the restacks unlink just as
        // thoroughly and did not, until this was added alongside them.
        if self.scene_is_being_walked() {
            return None;
        }
        let raw = self.owned_node_ptr(node)?;
        // SAFETY: a resolvable id names a live node.
        unsafe { sys::wlr_scene_node_raise_to_top(raw.as_ptr()) };
        Some(())
    }

    /// Make `node` the bottommost of its siblings. The mirror of
    /// [`raise_node_to_top`](Runtime::raise_node_to_top).
    pub fn lower_node_to_bottom(&self, node: NodeId) -> Option<()> {
        // Refused while a scene borrow or buffer walk is live. wlroots
        // iterates with `wl_list_for_each`, not the `_safe` variant, so
        // unlinking a node and reinserting it elsewhere mid-walk leaves the
        // iteration reading `link.next` from where the node used to be — it
        // silently stops early rather than crashing, which is worse. The
        // destroy calls refuse for this reason; the restacks unlink just as
        // thoroughly and did not, until this was added alongside them.
        if self.scene_is_being_walked() {
            return None;
        }
        let raw = self.owned_node_ptr(node)?;
        // SAFETY: a resolvable id names a live node.
        unsafe { sys::wlr_scene_node_lower_to_bottom(raw.as_ptr()) };
        Some(())
    }

    /// Move `node` under `new_parent`, keeping its own position.
    ///
    /// `None` — with nothing moved — when either id is unknown or stale, when
    /// `new_parent` is not a tree, when `node` is not one this crate created
    /// for the consumer, when a [`with_node`](Runtime::with_node) borrow or a
    /// [`for_each_buffer`](Runtime::for_each_buffer) walk is live, or when the
    /// move would make a cycle (`new_parent` is `node` itself or one of its
    /// descendants). wlroots asserts the cycle case, and an assert here is a
    /// process abort; a reparent mid-walk would unlink the node out of the very
    /// `wl_list` wlroots is iterating.
    ///
    /// Reparenting changes which destroy cascade owns the node, so the parent
    /// tracking the frozen [`RectId`]/[`BufferId`] tables still carry is
    /// recomputed for every row afterwards. Without that, a rect moved out of
    /// a toplevel's tree would still be purged when that toplevel died, and
    /// one moved *into* it would not be.
    pub fn reparent_node(&self, node: NodeId, new_parent: NodeId) -> Option<()> {
        if self.scene_is_being_walked() {
            return None;
        }
        if node == new_parent {
            return None;
        }
        let raw = self.owned_node_ptr(node)?;
        let parent = self.node_tree_ptr(new_parent)?;
        // SAFETY: both ids resolve, so both name live nodes; this walks
        // `parent` pointers, every one of which is a live tree or null.
        let cycles = unsafe {
            let mut cursor: *mut sys::wlr_scene_tree = parent.as_ptr();
            let mut hit = false;
            while !cursor.is_null() {
                if std::ptr::eq(&raw const (*cursor).node, raw.as_ptr()) {
                    hit = true;
                    break;
                }
                cursor = (*cursor).node.parent;
            }
            hit
        };
        if cycles {
            return None;
        }
        // SAFETY: `raw` and `parent` are live, and the walk above ruled out
        // exactly the cycle `wlr_scene_node_reparent` asserts against.
        unsafe { sys::wlr_scene_node_reparent(raw.as_ptr(), parent.as_ptr()) };
        self.reclassify_legacy_parents();
        Some(())
    }

    /// Recompute the parent tracking of every [`RectId`]/[`BufferId`] row.
    ///
    /// Those two id types predate [`NodeId`] and their tables purge by
    /// parentage rather than by addon, so a reparent that moved a rect (or an
    /// ancestor of one) between a toplevel's tree and a band leaves the
    /// recorded parent lying. Recomputing all of them is O(rows × depth) with
    /// a handful of rows and no FFI, which is cheaper than tracking exactly
    /// which subtree a reparent moved — and it cannot be subtly wrong.
    ///
    /// Deliberately walks each node's own ancestor chain rather than trusting
    /// what was recorded: that is the property being restored.
    fn reclassify_legacy_parents(&self) {
        // Copy the pointers out before classifying: `classify_parent` borrows
        // `tree_to_toplevel` and `graphics`, and no borrow of `rects`/
        // `buffers` may be held while another of this runtime's tables is
        // read.
        let rects: Vec<(RectId, NonNull<sys::wlr_scene_rect>)> = self
            .inner
            .rects
            .borrow()
            .iter()
            .map(|(id, entry)| (*id, entry.raw))
            .collect();
        let buffers: Vec<(BufferId, NonNull<sys::wlr_scene_buffer>)> = self
            .inner
            .buffers
            .borrow()
            .iter()
            .map(|(id, entry)| (*id, entry.node))
            .collect();

        let mut rect_parents = Vec::with_capacity(rects.len());
        for (id, raw) in rects {
            // SAFETY: a row in `rects` names a node wlroots has not destroyed
            // (see `set_rect_position`'s own comment), so its `parent` chain
            // is walkable.
            let parent = unsafe { self.classify_parent((*raw.as_ptr()).node.parent) };
            rect_parents.push((id, parent));
        }
        let mut buffer_parents = Vec::with_capacity(buffers.len());
        for (id, raw) in buffers {
            // SAFETY: as above, for `buffers`.
            let parent = unsafe { self.classify_parent((*raw.as_ptr()).node.parent) };
            buffer_parents.push((
                id,
                match parent {
                    RectParent::Toplevel(toplevel) => Some(toplevel),
                    RectParent::Root | RectParent::Band(_) => None,
                },
            ));
        }

        let mut table = self.inner.rects.borrow_mut();
        for (id, parent) in rect_parents {
            if let Some(entry) = table.get_mut(&id) {
                entry.parent = parent;
            }
        }
        drop(table);
        let mut table = self.inner.buffers.borrow_mut();
        for (id, parent) in buffer_parents {
            if let Some(entry) = table.get_mut(&id) {
                entry.parent = parent;
            }
        }
    }

    /// Which purge class a node parented under `tree` belongs to.
    ///
    /// Walks upward and takes the first answer: a toplevel's tree beats a
    /// band, because a node inside a toplevel dies with that toplevel even
    /// though the band it sits in outlives it.
    ///
    /// # Safety
    ///
    /// `tree` must be null or point at a live `wlr_scene_tree` whose ancestor
    /// chain is walkable.
    unsafe fn classify_parent(&self, tree: *mut sys::wlr_scene_tree) -> RectParent {
        let bands: [(Band, NonNull<sys::wlr_scene_tree>); 6] = {
            let g = self.inner.graphics.borrow();
            match g.as_ref() {
                Some(g) => [
                    (Band::Background, g.background_band),
                    (Band::Bottom, g.bottom_band),
                    (Band::Toplevel, g.toplevel_band),
                    (Band::Top, g.top_band),
                    (Band::Overlay, g.overlay_band),
                    (Band::Lock, g.lock_band),
                ],
                // No graphics means no bands and no toplevels either, so every
                // row can only be a root one.
                None => return RectParent::Root,
            }
        };

        let mut cursor = tree;
        while !cursor.is_null() {
            let found = self
                .inner
                .tree_to_toplevel
                .borrow()
                .get(&(cursor as usize))
                .copied();
            if let Some(toplevel) = found {
                return RectParent::Toplevel(toplevel);
            }
            if let Some((band, _)) = bands.iter().find(|(_, tree)| tree.as_ptr() == cursor) {
                return RectParent::Band(*band);
            }
            // SAFETY: the caller guarantees the chain is walkable; every
            // `parent` in a scene is a live tree or null at the root.
            cursor = unsafe { (*cursor).node.parent };
        }
        RectParent::Root
    }

    /// `node`'s layout-local coordinates.
    ///
    /// `None` when the id is unknown or stale, **and** when the node or any
    /// ancestor is disabled — wlroots reports the latter as a `false` return
    /// rather than a coordinate, and flattening the two into `(0, 0)` would
    /// lose exactly the distinction a caller checking visibility needs.
    pub fn node_coords(&self, node: NodeId) -> Option<(i32, i32)> {
        let raw = self.node_ptr(node)?;
        let mut lx = 0;
        let mut ly = 0;
        // SAFETY: a resolvable id names a live node; both out-parameters are
        // live stack locals.
        let found = unsafe { sys::wlr_scene_node_coords(raw.as_ptr(), &raw mut lx, &raw mut ly) };
        found.then_some((lx, ly))
    }

    /// `node`'s position relative to its parent. `None` for an unknown or
    /// stale id.
    pub fn node_position(&self, node: NodeId) -> Option<(i32, i32)> {
        let raw = self.node_ptr(node)?;
        // SAFETY: a resolvable id names a live node.
        Some(unsafe { ((*raw.as_ptr()).x, (*raw.as_ptr()).y) })
    }

    /// Whether `node` is enabled in itself — not whether it is visible, for
    /// which see [`node_coords`](Runtime::node_coords). `None` for an unknown
    /// or stale id.
    pub fn node_enabled(&self, node: NodeId) -> Option<bool> {
        let raw = self.node_ptr(node)?;
        // SAFETY: a resolvable id names a live node.
        Some(unsafe { (*raw.as_ptr()).enabled })
    }

    /// What `node` is. `None` for an unknown or stale id, or for a node type
    /// this build does not know.
    pub fn node_kind(&self, node: NodeId) -> Option<NodeKind> {
        let raw = self.node_ptr(node)?;
        // SAFETY: a resolvable id names a live node.
        NodeKind::from_raw(unsafe { (*raw.as_ptr()).type_ })
    }

    /// The id of `node`'s parent tree.
    ///
    /// `None` for an unknown or stale id, and at the scene root, which has no
    /// parent. The parent is given an id if it did not have one.
    pub fn node_parent(&self, node: NodeId) -> Option<NodeId> {
        let raw = self.node_ptr(node)?;
        // SAFETY: a resolvable id names a live node, and its `parent` is a
        // live tree or null at the root.
        unsafe {
            let parent = (*raw.as_ptr()).parent;
            if parent.is_null() {
                return None;
            }
            self.ensure_node_id(&raw mut (*parent).node, NodeOrigin::Foreign)
        }
    }

    /// `node`'s children, bottom to top.
    ///
    /// Sibling order **is** stacking order — wlroots appends each new child at
    /// the end — so the last element is the topmost. Every child gets an id if
    /// it did not have one, so a toplevel's tree living in a band shows up
    /// here as a read-only foreign node rather than being silently skipped.
    ///
    /// `None` when the id is unknown, stale, or not a tree.
    pub fn node_children(&self, node: NodeId) -> Option<Vec<NodeId>> {
        let tree = self.node_tree_ptr(node)?;
        // Collect the raw children before minting any id: `ensure_node_id`
        // calls into wlroots, and `wl_list_iter`'s contract forbids disturbing
        // the list ahead of its cursor while one is outstanding.
        let mut raws = Vec::new();
        // SAFETY: a resolvable id names a live tree, so `children` is an
        // initialised list head whose entries are live `wlr_scene_node`s, and
        // nothing in this loop modifies the list.
        unsafe {
            for child in sys::wl_list_iter::<sys::wlr_scene_node>::new(
                &raw mut (*tree.as_ptr()).children,
                std::mem::offset_of!(sys::wlr_scene_node, link),
            ) {
                raws.push(child);
            }
        }
        let mut out = Vec::with_capacity(raws.len());
        for child in raws {
            // SAFETY: `child` was just read out of a live tree's child list,
            // and nothing since could have freed it — no wlroots call emitting
            // a signal has run, and `ensure_node_id` only touches addon sets.
            if let Some(id) = unsafe { self.ensure_node_id(child, NodeOrigin::Foreign) } {
                out.push(id);
            }
        }
        Some(out)
    }

    /// The topmost node at layout coordinates `(x, y)`, with the coordinates
    /// relative to that node.
    ///
    /// Surface nodes respect their input region, so this answers "what would
    /// receive a click here" rather than "what is painted here". The struck
    /// node gets an id if it did not have one, which is how a client's own
    /// surface node — one this crate never created — becomes nameable.
    ///
    /// `None` when nothing is there, or before
    /// [`init_graphics`](Runtime::init_graphics) has run.
    ///
    /// This is **not** the query to forward input with:
    /// [`toplevel_at`](Runtime::toplevel_at) is, because a hit on a popup's
    /// surface node reports that node rather than the window it belongs to.
    pub fn node_at(&self, x: f64, y: f64) -> Option<(NodeId, f64, f64)> {
        let scene = self.scene_ptr()?;
        let mut nx = 0.0;
        let mut ny = 0.0;
        // SAFETY: the scene is this runtime's own and outlives the call; both
        // out-parameters are live stack locals.
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
        // SAFETY: wlroots returned a node belonging to this scene, so it is
        // live.
        let id = unsafe { self.ensure_node_id(node, NodeOrigin::Foreign) }?;
        Some((id, nx, ny))
    }

    /// Visit every buffer node in `node`'s subtree, in render order.
    ///
    /// Root to leaves, which is back to front. `f` receives each buffer node's
    /// id and its layout-local position.
    ///
    /// `None` for an unknown or stale id.
    ///
    /// While the walk is running nothing may free or move a node underneath it:
    /// [`destroy_node`](Runtime::destroy_node),
    /// [`reparent_node`](Runtime::reparent_node),
    /// [`remove_rect`](Runtime::remove_rect) and
    /// [`remove_buffer`](Runtime::remove_buffer) all return `None` without
    /// acting when called from inside `f`, on this runtime or on any clone of
    /// it — the same guard [`with_node`](Runtime::with_node) raises, and for a
    /// sharper reason. wlroots walks each tree's child list with
    /// `wl_list_for_each`, **not** the `_safe` form: it reads the current
    /// node's `link.next` *after* `f` returns, so destroying the node `f` was
    /// just handed (or any ancestor of it, which frees the list heads the walk
    /// is standing in) is a use-after-free inside wlroots' own recursion.
    ///
    /// **Creating** a node is refused for the same reason, which this doc used
    /// to say the opposite of. Appending to the tree the cursor is standing in
    /// rewires the `next` it is about to read, so the walk never reaches the
    /// end: `rt.for_each_buffer(t, |..| { rt.create_scene_buffer(t, None); })`
    /// allocates without bound from entirely safe code. It was true that a
    /// tree the walk has *not reached* is harmless — but nothing here can tell
    /// the caller which trees those are, so the permission could not be acted
    /// on safely and is withdrawn.
    ///
    /// # Panics
    ///
    /// A panic escaping `f` is caught, the remaining nodes are skipped, and
    /// the panic is resumed once wlroots' own iteration has returned — an
    /// unwind straight out of the `extern "C"` frame wlroots calls `f` from
    /// would abort the process instead.
    pub fn for_each_buffer(&self, node: NodeId, f: impl FnMut(NodeId, i32, i32)) -> Option<()> {
        let raw = self.node_ptr(node)?;
        self.buffer_walk(f, |iterator, user_data| {
            // SAFETY: a resolvable id names a live node; the iterator has
            // `wlr_scene_buffer_iterator_func_t`'s signature and `user_data`
            // outlives the call, both guaranteed by `buffer_walk`.
            unsafe { sys::wlr_scene_node_for_each_buffer(raw.as_ptr(), iterator, user_data) };
        });
        Some(())
    }

    /// The body [`for_each_buffer`](Runtime::for_each_buffer) and
    /// [`scene_output_for_each_buffer`](Runtime::scene_output_for_each_buffer)
    /// share: one visitor trampoline, one borrow guard, one panic hand-off.
    ///
    /// `run` performs the wlroots call, handed the C iterator and the erased
    /// context to pass through. It must not do anything else with either.
    fn buffer_walk(
        &self,
        f: impl FnMut(NodeId, i32, i32),
        run: impl FnOnce(sys::wlr_scene_buffer_iterator_func_t, *mut std::ffi::c_void),
    ) {
        struct Ctx<'a> {
            runtime: &'a Runtime,
            f: &'a mut dyn FnMut(NodeId, i32, i32),
            panic: Option<Box<dyn std::any::Any + Send + 'static>>,
        }

        unsafe extern "C" fn visit(
            buffer: *mut sys::wlr_scene_buffer,
            sx: std::os::raw::c_int,
            sy: std::os::raw::c_int,
            user_data: *mut std::ffi::c_void,
        ) {
            // SAFETY: `user_data` is the `&mut Ctx` handed to
            // `wlr_scene_node_for_each_buffer` below, which wlroots calls this
            // from synchronously, so the borrow is live and unaliased.
            // `buffer` is a live node of the subtree being walked.
            unsafe {
                let ctx = &mut *user_data.cast::<Ctx<'_>>();
                if ctx.panic.is_some() {
                    return;
                }
                let Some(id) = ctx
                    .runtime
                    .ensure_node_id(&raw mut (*buffer).node, NodeOrigin::Foreign)
                else {
                    return;
                };
                // The closure is a consumer's, so it may panic, and this frame
                // is `extern "C"`, where an unwind aborts. Catch here and
                // resume after wlroots has finished walking.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (ctx.f)(id, sx, sy)))
                {
                    Ok(()) => {}
                    Err(payload) => ctx.panic = Some(payload),
                }
            }
        }

        let mut f = f;
        let mut ctx = Ctx {
            runtime: self,
            f: &mut f,
            panic: None,
        };
        // Named through the C typedef rather than passed inline: that is what
        // makes the compiler check `visit`'s signature against wlroots' own
        // `wlr_scene_buffer_iterator_func_t` instead of against whatever
        // `wlr_scene_node_for_each_buffer`'s parameter happens to be spelled
        // as today.
        let iterator: sys::wlr_scene_buffer_iterator_func_t = Some(visit);
        // Raised for the whole walk, and lowered by `Drop` even if `f` panicked
        // (the panic is caught above and resumed below, so the guard falls with
        // this frame either way). wlroots iterates with `wl_list_for_each`, so
        // a node freed or unlinked by `f` leaves the walk reading `link.next`
        // out of reclaimed memory — see the callers' own docs.
        let _guard = NodeBorrowGuard::enter(&self.inner);
        // The borrow guard only makes *this crate's* by-id destroys refuse. It
        // says nothing to wlroots, which frees nodes on its own schedule the
        // moment the event loop runs — and the closure can drive that loop, by
        // holding a `&Display` or an `&EventLoop`. A client unmapping its
        // window mid-closure would then free the subtree under a live handle.
        // `ForeignFrame` sets the same flag a real handler delivery does, so
        // `EventLoop::dispatch` refuses for the life of the borrow and that
        // window cannot open.
        let _frame = crate::dispatch::ForeignFrame::enter();
        run(iterator, (&raw mut ctx).cast::<std::ffi::c_void>());
        if let Some(payload) = ctx.panic {
            std::panic::resume_unwind(payload);
        }
    }

    /// Borrow `node` as a [`SceneNode`] handle for the duration of `f`.
    ///
    /// The handle cannot escape `f` — that is what its lifetime is for — and
    /// while it is live nothing may free the node underneath it:
    /// [`destroy_node`](Runtime::destroy_node),
    /// [`reparent_node`](Runtime::reparent_node),
    /// [`remove_rect`](Runtime::remove_rect) and
    /// [`remove_buffer`](Runtime::remove_buffer) all return `None` without
    /// acting when called from inside `f`, on this runtime or on any clone of
    /// it. Nesting borrows is fine; they are read-only.
    ///
    /// Those refusals are only half of it, because they bind this crate and
    /// wlroots frees nodes on its own schedule. The other half is that
    /// [`EventLoop::dispatch`](crate::EventLoop::dispatch) also refuses for
    /// the life of the borrow: without that, a closure holding a `&Display`
    /// could drive the loop, a client could unmap its window, and wlroots
    /// would free the subtree under a handle that is still live — with no
    /// `unsafe` written anywhere.
    ///
    /// `None`, without calling `f`, for an unknown or stale id.
    pub fn with_node<R>(&self, node: NodeId, f: impl FnOnce(&SceneNode<'_>) -> R) -> Option<R> {
        let raw = self.node_ptr(node)?;
        // Raised before the handle is minted and lowered by `Drop`, so a panic
        // escaping `f` cannot leave the runtime permanently refusing destroys.
        let _guard = NodeBorrowGuard::enter(&self.inner);
        // The borrow guard only makes *this crate's* by-id destroys refuse. It
        // says nothing to wlroots, which frees nodes on its own schedule the
        // moment the event loop runs — and the closure can drive that loop, by
        // holding a `&Display` or an `&EventLoop`. A client unmapping its
        // window mid-closure would then free the subtree under a live handle.
        // `ForeignFrame` sets the same flag a real handler delivery does, so
        // `EventLoop::dispatch` refuses for the life of the borrow and that
        // window cannot open.
        let _frame = crate::dispatch::ForeignFrame::enter();
        // SAFETY: a resolvable id names a live node, and the guard above is
        // what keeps it live for the whole of `f` — every call that could free
        // it refuses while the guard is held.
        let handle = unsafe { SceneNode::from_raw_with_id(raw.as_ptr(), node) };
        Some(f(&handle))
    }

    /// Borrow `node` as a [`SceneTree`] handle, if it is a tree.
    ///
    /// The [`with_node`](Runtime::with_node) contract applies verbatim,
    /// including the destroy refusals while the borrow is live. `None`,
    /// without calling `f`, when the id is unknown, stale, or not a tree.
    pub fn with_tree<R>(&self, node: NodeId, f: impl FnOnce(&SceneTree<'_>) -> R) -> Option<R> {
        let raw = self.node_tree_ptr(node)?;
        let _guard = NodeBorrowGuard::enter(&self.inner);
        // The borrow guard only makes *this crate's* by-id destroys refuse. It
        // says nothing to wlroots, which frees nodes on its own schedule the
        // moment the event loop runs — and the closure can drive that loop, by
        // holding a `&Display` or an `&EventLoop`. A client unmapping its
        // window mid-closure would then free the subtree under a live handle.
        // `ForeignFrame` sets the same flag a real handler delivery does, so
        // `EventLoop::dispatch` refuses for the life of the borrow and that
        // window cannot open.
        let _frame = crate::dispatch::ForeignFrame::enter();
        // SAFETY: as in `with_node`; `node_tree_ptr` additionally checked the
        // node's tag, which is `wlr_scene_tree_from_node`'s precondition.
        let handle = unsafe { SceneTree::from_raw_with_id(raw.as_ptr(), node) };
        Some(f(&handle))
    }

    /// Borrow `node` as a [`SceneRect`] handle, if it is a rect.
    ///
    /// The [`with_node`](Runtime::with_node) contract applies verbatim.
    pub fn with_rect<R>(&self, node: NodeId, f: impl FnOnce(&SceneRect<'_>) -> R) -> Option<R> {
        let raw = self.node_ptr(node)?;
        // SAFETY: a resolvable id names a live node.
        if NodeKind::from_raw(unsafe { (*raw.as_ptr()).type_ }) != Some(NodeKind::Rect) {
            return None;
        }
        let _guard = NodeBorrowGuard::enter(&self.inner);
        // The borrow guard only makes *this crate's* by-id destroys refuse. It
        // says nothing to wlroots, which frees nodes on its own schedule the
        // moment the event loop runs — and the closure can drive that loop, by
        // holding a `&Display` or an `&EventLoop`. A client unmapping its
        // window mid-closure would then free the subtree under a live handle.
        // `ForeignFrame` sets the same flag a real handler delivery does, so
        // `EventLoop::dispatch` refuses for the life of the borrow and that
        // window cannot open.
        let _frame = crate::dispatch::ForeignFrame::enter();
        // SAFETY: as in `with_node`, plus the tag check just above, which is
        // `wlr_scene_rect_from_node`'s precondition.
        let handle = unsafe {
            SceneRect::from_raw_with_id(sys::wlr_scene_rect_from_node(raw.as_ptr()), node)
        };
        Some(f(&handle))
    }

    /// Borrow `node` as a [`SceneBuffer`] handle, if it is a buffer node.
    ///
    /// The [`with_node`](Runtime::with_node) contract applies verbatim.
    pub fn with_scene_buffer<R>(
        &self,
        node: NodeId,
        f: impl FnOnce(&SceneBuffer<'_>) -> R,
    ) -> Option<R> {
        let raw = self.node_ptr(node)?;
        // SAFETY: a resolvable id names a live node.
        if NodeKind::from_raw(unsafe { (*raw.as_ptr()).type_ }) != Some(NodeKind::Buffer) {
            return None;
        }
        let _guard = NodeBorrowGuard::enter(&self.inner);
        // The borrow guard only makes *this crate's* by-id destroys refuse. It
        // says nothing to wlroots, which frees nodes on its own schedule the
        // moment the event loop runs — and the closure can drive that loop, by
        // holding a `&Display` or an `&EventLoop`. A client unmapping its
        // window mid-closure would then free the subtree under a live handle.
        // `ForeignFrame` sets the same flag a real handler delivery does, so
        // `EventLoop::dispatch` refuses for the life of the borrow and that
        // window cannot open.
        let _frame = crate::dispatch::ForeignFrame::enter();
        // SAFETY: as in `with_node`, plus the tag check just above.
        let handle = unsafe {
            SceneBuffer::from_raw_with_id(sys::wlr_scene_buffer_from_node(raw.as_ptr()), node)
        };
        Some(f(&handle))
    }

    /// Create a solid-colour rect under `parent`, in the **premultiplied**
    /// RGBA [`add_rect`](Runtime::add_rect) takes.
    ///
    /// Unlike [`add_rect`](Runtime::add_rect), a negative dimension is refused
    /// rather than handed to wlroots, which asserts on one. That asymmetry is
    /// deliberate: `add_rect`'s signature was published in 0.20.1 without the
    /// guard and cannot gain one within this wlroots minor (see
    /// [`set_rect_size`](Runtime::set_rect_size)'s own doc); this method is new
    /// and starts out right.
    ///
    /// `None` if `parent` is unknown, stale or not a tree, if either dimension
    /// is negative, or if wlroots could not create the node.
    pub fn create_rect(
        &self,
        parent: NodeId,
        width: i32,
        height: i32,
        color: [f32; 4],
    ) -> Option<NodeId> {
        // Refused while a node borrow or a scene walk is live.
        //
        // The borrow gate was added to every call that *unlinks* a node and to
        // none that *inserts* one, which left the more dangerous half open:
        // wlroots walks with `wl_list_for_each`, not the `_safe` variant, so
        // its cursor holds a raw `next` pointer. Unlinking mid-walk was
        // refused; appending rewires the tail the cursor is about to reach,
        // and `for_each_buffer` then never terminates —
        // `rt.for_each_buffer(t, |..| { rt.create_scene_buffer(t, None); })`
        // allocates without bound, from entirely safe code.
        //
        // Refused for any live borrow rather than only for the tree the walk
        // is standing in, because which tree the cursor has reached is not
        // knowable from here — and a rule that holds only sometimes is the
        // one that gets relied on.
        if self.scene_is_being_walked() {
            return None;
        }
        if width < 0 || height < 0 {
            return None;
        }
        let tree = self.node_tree_ptr(parent)?;
        // SAFETY: a resolvable id names a live tree; `color` is a live
        // four-float array for the call, which wlroots copies.
        let rect =
            unsafe { sys::wlr_scene_rect_create(tree.as_ptr(), width, height, color.as_ptr()) };
        let rect = NonNull::new(rect)?;
        // SAFETY: `rect` is the node wlroots just created, so nothing has had
        // the chance to attach a payload of this kind to it.
        unsafe { self.record_node(&raw mut (*rect.as_ptr()).node, NodeOrigin::Owned, None) }
    }

    /// Resize a rect node.
    ///
    /// `None` if the id is unknown, stale, not a rect, names a band or the
    /// scene root, or either dimension is negative —
    /// `wlr_scene_rect_set_size` asserts non-negative and an assert is a
    /// process abort.
    pub fn set_node_rect_size(&self, node: NodeId, width: i32, height: i32) -> Option<()> {
        if width < 0 || height < 0 {
            return None;
        }
        let raw = self.movable_rect_ptr(node)?;
        // SAFETY: a resolvable id of the right tag names a live rect, and the
        // dimensions were checked against wlroots' own assert just above.
        unsafe { sys::wlr_scene_rect_set_size(raw.as_ptr(), width, height) };
        Some(())
    }

    /// Recolour a rect node, in the same premultiplied RGBA
    /// [`add_rect`](Runtime::add_rect) takes. `None` if the id is unknown,
    /// stale, not a rect, or not a node this crate created for you — every
    /// rect node in the scene is one this crate made, so the last case only
    /// arises for an id naming something else.
    pub fn set_node_rect_color(&self, node: NodeId, color: [f32; 4]) -> Option<()> {
        let raw = self.movable_rect_ptr(node)?;
        // SAFETY: as for `set_node_rect_size`; `color` is live for the call
        // and wlroots copies it rather than retaining the pointer.
        unsafe { sys::wlr_scene_rect_set_color(raw.as_ptr(), color.as_ptr()) };
        Some(())
    }

    /// Create a buffer node under `parent`, optionally showing `buffer`.
    ///
    /// A node with no buffer is legal and useful: it draws nothing until
    /// [`set_scene_buffer`](Runtime::set_scene_buffer) gives it pixels.
    ///
    /// The buffer is **borrowed**. wlroots takes its own lock on it and this
    /// call does not release the caller's reference, unlike
    /// [`add_buffer`](Runtime::add_buffer), which owns the pixels it uploads —
    /// see `buffer.rs`'s "Refcount story" for what the two references mean.
    ///
    /// `None` if `parent` is unknown, stale or not a tree, or if wlroots could
    /// not create the node.
    pub fn create_scene_buffer(
        &self,
        parent: NodeId,
        buffer: Option<&Buffer<'_>>,
    ) -> Option<NodeId> {
        // Refused while a node borrow or a scene walk is live.
        //
        // The borrow gate was added to every call that *unlinks* a node and to
        // none that *inserts* one, which left the more dangerous half open:
        // wlroots walks with `wl_list_for_each`, not the `_safe` variant, so
        // its cursor holds a raw `next` pointer. Unlinking mid-walk was
        // refused; appending rewires the tail the cursor is about to reach,
        // and `for_each_buffer` then never terminates —
        // `rt.for_each_buffer(t, |..| { rt.create_scene_buffer(t, None); })`
        // allocates without bound, from entirely safe code.
        //
        // Refused for any live borrow rather than only for the tree the walk
        // is standing in, because which tree the cursor has reached is not
        // knowable from here — and a rule that holds only sometimes is the
        // one that gets relied on.
        if self.scene_is_being_walked() {
            return None;
        }
        let tree = self.node_tree_ptr(parent)?;
        let buf = buffer.map_or(std::ptr::null_mut(), |b| b.as_ptr());
        // SAFETY: a resolvable id names a live tree; `buf` is null or a live
        // buffer the caller's own reference keeps alive across this call, and
        // wlroots takes its own lock on it.
        let node = unsafe { sys::wlr_scene_buffer_create(tree.as_ptr(), buf) };
        let node = NonNull::new(node)?;
        // SAFETY: `node` is the node wlroots just created.
        unsafe { self.record_node(&raw mut (*node.as_ptr()).node, NodeOrigin::Owned, None) }
    }

    /// Replace what a buffer node shows.
    ///
    /// `buffer` is borrowed, exactly as in
    /// [`create_scene_buffer`](Runtime::create_scene_buffer); `None` clears
    /// the node. `options` carries the damage hint and the explicit-sync wait
    /// point.
    ///
    /// `None` if the id is unknown, stale or not a buffer node, or if `buffer`
    /// is `None` while `options` carries a damage region —
    /// `wlr_scene_buffer_set_buffer_with_options` asserts
    /// `buffer || !options->damage`, and an assert is a process abort. A damage
    /// region is in buffer-local coordinates, so with no buffer there is
    /// nothing to scale it by; clearing a node and damaging it are separate
    /// requests.
    ///
    /// Also `None` for a node this crate did not create for you — a client's
    /// own surface node from [`node_at`](Runtime::node_at), say. Unlike the
    /// appearance setters, replacing the *buffer* of a node wlroots is filling
    /// from a surface is not a change to how it looks, it is taking the
    /// content away from wlroots, which refills it on the client's next
    /// commit.
    pub fn set_scene_buffer(
        &self,
        node: NodeId,
        buffer: Option<&Buffer<'_>>,
        options: &SceneBufferOptions<'_>,
    ) -> Option<()> {
        if buffer.is_none() && options.has_damage() {
            return None;
        }
        let raw = self.movable_scene_buffer_ptr(node)?;
        let buf = buffer.map_or(std::ptr::null_mut(), |b| b.as_ptr());
        let opts = options.as_c();
        // SAFETY: a resolvable id of the right tag names a live buffer node;
        // `buf` is null or a live buffer; `opts` borrows the region and the
        // timeline from `options`, both of which outlive this call.
        unsafe {
            sys::wlr_scene_buffer_set_buffer_with_options(raw.as_ptr(), buf, &raw const opts)
        };
        Some(())
    }

    /// Declare which part of a buffer node is fully opaque.
    ///
    /// An optimisation hint: wlroots may skip drawing whatever is behind it.
    /// `None` clears the hint. Over-declaring leaves stale pixels on screen
    /// rather than producing an error, so this is a promise, not a request.
    ///
    /// Returns `None` if the id is unknown, stale or not a buffer node.
    pub fn set_scene_buffer_opaque_region(
        &self,
        node: NodeId,
        region: Option<&Region>,
    ) -> Option<()> {
        // Refused while a node borrow is live, unlike its sibling appearance
        // setters, because this one *frees memory a live handle can be
        // pointing into*. `SceneBuffer::opaque_region` hands out a
        // `RegionRef` borrowed from the node's embedded `pixman_region32`,
        // and `wlr_scene_buffer_set_opaque_region` copies over it —
        // `pixman_region32_copy` frees the old box array, so a `RegionRef`
        // iterator taken earlier in the same closure reads freed memory.
        //
        // `NodeBorrowGuard` keeps the *node* alive and says nothing about
        // that: the node is fine, its region's heap block is not. Every other
        // appearance setter writes a scalar into the node and is safe to leave
        // open.
        if self.scene_is_being_walked() {
            return None;
        }
        let raw = self.restylable_scene_buffer_ptr(node)?;
        let ptr = region.map_or(std::ptr::null(), |r| r.as_ptr());
        // SAFETY: a resolvable id of the right tag names a live buffer node;
        // `ptr` is null or a live region wlroots copies out of.
        unsafe { sys::wlr_scene_buffer_set_opaque_region(raw.as_ptr(), ptr) };
        Some(())
    }

    /// Crop a buffer node to `source`, in buffer-local coordinates. `None`
    /// samples the whole buffer, which is the default.
    ///
    /// Returns `None` if the id is unknown, stale or not a buffer node, or if
    /// any of `source`'s four fields is negative or `NaN`.
    /// `wlr_scene_buffer_set_source_box` asserts all four are `>= 0`, and the
    /// comparison it compiles to (`comisd`/`jb`) fails on an unordered operand
    /// too, so a `NaN` aborts exactly as `-1.0` does.
    pub fn set_scene_buffer_source_box(&self, node: NodeId, source: Option<FBox>) -> Option<()> {
        if let Some(b) = source {
            // `v >= 0.0` rather than `!(v < 0.0)`: `NaN` fails every comparison,
            // so this refuses it, which is what the C does too.
            if ![b.x, b.y, b.width, b.height].iter().all(|v| *v >= 0.0) {
                return None;
            }
        }
        let raw = self.restylable_scene_buffer_ptr(node)?;
        let ptr = source.as_ref().map_or(std::ptr::null(), FBox::as_c);
        // SAFETY: a resolvable id of the right tag names a live buffer node;
        // `ptr` is null or points at `source`, a live local for this call,
        // which wlroots copies out of.
        unsafe { sys::wlr_scene_buffer_set_source_box(raw.as_ptr(), ptr) };
        Some(())
    }

    /// Scale a buffer node's on-screen size independently of its pixel size.
    ///
    /// Zero means "use the buffer's own size", which is wlroots' documented
    /// default rather than an error. A negative dimension is refused:
    /// `wlr_scene_buffer_set_dest_size` asserts non-negative.
    ///
    /// `None` if the id is unknown, stale or not a buffer node.
    pub fn set_scene_buffer_dest_size(&self, node: NodeId, width: i32, height: i32) -> Option<()> {
        if width < 0 || height < 0 {
            return None;
        }
        let raw = self.restylable_scene_buffer_ptr(node)?;
        // SAFETY: a resolvable id of the right tag names a live buffer node,
        // and the dimensions were checked against wlroots' assert above.
        unsafe { sys::wlr_scene_buffer_set_dest_size(raw.as_ptr(), width, height) };
        Some(())
    }

    /// Apply a transform to a buffer node's contents. `None` if the id is
    /// unknown, stale or not a buffer node.
    pub fn set_scene_buffer_transform(&self, node: NodeId, transform: Transform) -> Option<()> {
        let raw = self.restylable_scene_buffer_ptr(node)?;
        // SAFETY: a resolvable id of the right tag names a live buffer node.
        unsafe { sys::wlr_scene_buffer_set_transform(raw.as_ptr(), transform.into()) };
        Some(())
    }

    /// Set a buffer node's opacity multiplier.
    ///
    /// `None` if the id is unknown, stale or not a buffer node, or if
    /// `opacity` is outside `0.0..=1.0` — including `NaN`, which no comparison
    /// accepts. wlroots does not range-check, and an out-of-range multiplier
    /// renders as a silently wrong image rather than as an error.
    pub fn set_scene_buffer_opacity(&self, node: NodeId, opacity: f32) -> Option<()> {
        if !(0.0..=1.0).contains(&opacity) {
            return None;
        }
        let raw = self.restylable_scene_buffer_ptr(node)?;
        // SAFETY: a resolvable id of the right tag names a live buffer node.
        unsafe { sys::wlr_scene_buffer_set_opacity(raw.as_ptr(), opacity) };
        Some(())
    }

    /// Choose how a buffer node is sampled when scaled. `None` if the id is
    /// unknown, stale or not a buffer node.
    pub fn set_scene_buffer_filter(&self, node: NodeId, filter: FilterMode) -> Option<()> {
        let raw = self.restylable_scene_buffer_ptr(node)?;
        // SAFETY: a resolvable id of the right tag names a live buffer node.
        unsafe { sys::wlr_scene_buffer_set_filter_mode(raw.as_ptr(), filter.into()) };
        Some(())
    }

    /// Declare a buffer node's electro-optical transfer function. `None` if
    /// the id is unknown, stale or not a buffer node.
    pub fn set_scene_buffer_transfer_function(
        &self,
        node: NodeId,
        transfer_function: TransferFunction,
    ) -> Option<()> {
        let raw = self.restylable_scene_buffer_ptr(node)?;
        // SAFETY: a resolvable id of the right tag names a live buffer node.
        unsafe {
            sys::wlr_scene_buffer_set_transfer_function(raw.as_ptr(), transfer_function.into());
        }
        Some(())
    }

    /// Declare a buffer node's colour primaries.
    ///
    /// Takes a *named* colour volume rather than a
    /// [`ColorPrimaries`](crate::ColorPrimaries): the C setter's parameter is
    /// `enum wlr_color_named_primaries`, not the full chromaticity struct.
    ///
    /// `None` if the id is unknown, stale or not a buffer node.
    pub fn set_scene_buffer_primaries(
        &self,
        node: NodeId,
        primaries: NamedPrimaries,
    ) -> Option<()> {
        let raw = self.restylable_scene_buffer_ptr(node)?;
        // SAFETY: a resolvable id of the right tag names a live buffer node.
        unsafe { sys::wlr_scene_buffer_set_primaries(raw.as_ptr(), primaries.into()) };
        Some(())
    }

    /// Declare the matrix coefficients a buffer node's YCbCr encoding uses.
    /// `None` if the id is unknown, stale or not a buffer node.
    pub fn set_scene_buffer_color_encoding(
        &self,
        node: NodeId,
        encoding: ColorEncoding,
    ) -> Option<()> {
        let raw = self.restylable_scene_buffer_ptr(node)?;
        // SAFETY: a resolvable id of the right tag names a live buffer node.
        unsafe { sys::wlr_scene_buffer_set_color_encoding(raw.as_ptr(), encoding.into()) };
        Some(())
    }

    /// Declare whether a buffer node's encoding uses the full or the limited
    /// value range. `None` if the id is unknown, stale or not a buffer node.
    pub fn set_scene_buffer_color_range(&self, node: NodeId, range: ColorRange) -> Option<()> {
        let raw = self.restylable_scene_buffer_ptr(node)?;
        // SAFETY: a resolvable id of the right tag names a live buffer node.
        unsafe { sys::wlr_scene_buffer_set_color_range(raw.as_ptr(), range.into()) };
        Some(())
    }

    // ---------------------------------------------------------------------
    // Scene-buffer observation (0.20.19)
    // ---------------------------------------------------------------------

    /// Install the live run's observer hook. Called only by `run_inner`.
    pub(crate) fn set_scene_observer(&self, observer: SceneObserver) {
        self.inner.scene_observer.set(Some(observer));
    }

    /// Clear it again, which `run_inner`'s guard does on every exit path.
    pub(crate) fn clear_scene_observer(&self) {
        self.inner.scene_observer.set(None);
        // The snapshots go with it: they name scene outputs by id, which stay
        // valid, but nothing can refresh them once the listeners are gone, and
        // a stale set answered after the run had ended would be worse than a
        // miss.
        self.inner.scene_buffer_outputs.borrow_mut().clear();
    }

    /// `id`'s node as a buffer node, if changing how it *looks* is allowed.
    ///
    /// Accepts a foreign node, unlike
    /// [`movable_scene_buffer_ptr`](Runtime::movable_scene_buffer_ptr). The
    /// Owned-only rule exists for one reason — this crate keeps placement
    /// bookkeeping for the nodes it hands out ids to, and a direct move would
    /// invalidate it behind `set_toplevel_position` and `raise_toplevel`'s
    /// backs. None of the appearance setters touch that bookkeeping: opacity,
    /// filter, transform, the colour metadata, the source box, the
    /// destination size and the opaque region change what a node looks like,
    /// not where it sits or what it sits above.
    ///
    /// Applying the placement rule to them made the single most ordinary
    /// compositor operation — fading a client's window, from the `NodeId`
    /// `node_at` just returned — come back `None` with no diagnostic, and
    /// contradicted each of those methods' own documented `None` cases
    /// ("unknown, stale, or not a buffer node").
    ///
    /// Note that wlroots itself sets several of these on a
    /// `wlr_scene_surface`'s buffer node on every surface commit, so a value
    /// written to a client's node may not survive the client's next frame.
    /// That is wlroots' behaviour showing through, not a refusal, and the
    /// caller can see it.
    ///
    /// `set_scene_buffer_buffer_with_options` deliberately does **not** use
    /// this: replacing the buffer of a node wlroots is filling from a surface
    /// is not an appearance change, it is fighting wlroots for ownership of
    /// the content.
    fn restylable_scene_buffer_ptr(&self, id: NodeId) -> Option<NonNull<sys::wlr_scene_buffer>> {
        self.scene_buffer_ptr(id)
    }

    /// `id`'s node as a buffer node, whoever owns it.
    ///
    /// Unlike [`movable_scene_buffer_ptr`](Runtime::movable_scene_buffer_ptr)
    /// this accepts a foreign node: observing a client's own surface node is
    /// the main thing anyone wants to do with these signals, and observation
    /// mutates nothing.
    fn scene_buffer_ptr(&self, id: NodeId) -> Option<NonNull<sys::wlr_scene_buffer>> {
        let raw = self.node_ptr(id)?;
        // SAFETY: a resolvable id names a live node.
        if NodeKind::from_raw(unsafe { (*raw.as_ptr()).type_ }) != Some(NodeKind::Buffer) {
            return None;
        }
        // SAFETY: the tag says this node is a buffer node.
        NonNull::new(unsafe { sys::wlr_scene_buffer_from_node(raw.as_ptr()) })
    }

    /// Start delivering `node`'s scene-buffer signals to the handler.
    ///
    /// Until this is called for a given node, none of the five
    /// [`OutputHandler`](crate::OutputHandler) scene-buffer methods fire for
    /// it. That is opt-in on purpose: these signals are per node, a scene holds
    /// as many buffer nodes as there are mapped surfaces, and linking six
    /// listeners into every one of them to deliver events nobody asked for
    /// would be a cost with no buyer.
    ///
    /// `node` may be any buffer node, including a client's own surface node
    /// found with [`node_at`](Runtime::node_at) — observation mutates nothing.
    /// Calling this twice for the same node is harmless and links nothing the
    /// second time, so a handler may call it unconditionally.
    ///
    /// The listeners belong to the [`Backend::run_all`](crate::Backend::run_all)
    /// call that was running when this was called, and are unlinked when it
    /// returns; a later run does not re-establish them.
    ///
    /// `None`, having linked nothing, when the id is unknown, stale or not a
    /// buffer node, or when **no `run_all` call is running** — there is no
    /// handler to deliver to, and no session to own the listeners.
    /// [`Backend::run`](crate::Backend::run) does not count: its handler bound
    /// is [`OutputHandler`](crate::OutputHandler) alone, and it installs no
    /// observer.
    pub fn observe_scene_buffer(&self, node: NodeId) -> Option<()> {
        let raw = self.scene_buffer_ptr(node)?;
        let observer = self.inner.scene_observer.get()?;
        // SAFETY: `session` and `watch` are the pair `run_inner` installed
        // together, so the erased pointer is the `Session` `watch` was
        // instantiated for and is live for as long as the hook is installed —
        // which is this call. `raw` is a live buffer node carrying `node` as
        // its id, which is what `scene_buffer_ptr` just established.
        unsafe { (observer.watch)(observer.session, node, raw.as_ptr()) };
        Some(())
    }

    /// Stop delivering `node`'s scene-buffer signals.
    ///
    /// `None` when no run is running or the node was not being observed. A node
    /// that is destroyed stops being observed on its own — the listeners are
    /// unlinked from inside its destroy emission — so this is for a compositor
    /// that has simply stopped caring.
    pub fn unobserve_scene_buffer(&self, node: NodeId) -> Option<()> {
        let observer = self.inner.scene_observer.get()?;
        // SAFETY: as in `observe_scene_buffer`.
        let watched = unsafe { (observer.is_watching)(observer.session, node) };
        if !watched {
            return None;
        }
        // SAFETY: as above.
        unsafe { (observer.unwatch)(observer.session, node) };
        self.forget_scene_buffer_outputs(node);
        Some(())
    }

    /// Whether `node`'s scene-buffer signals are being delivered.
    pub fn scene_buffer_observed(&self, node: NodeId) -> bool {
        let Some(observer) = self.inner.scene_observer.get() else {
            return false;
        };
        // SAFETY: as in `observe_scene_buffer`.
        unsafe { (observer.is_watching)(observer.session, node) }
    }

    /// The scene outputs `node` is currently displayed on.
    ///
    /// The payload
    /// [`OutputHandler::scene_buffer_outputs_update`](crate::OutputHandler::scene_buffer_outputs_update)
    /// does not carry, because wlroots hands that signal an array valid only
    /// for its own emission and this crate's events carry ids and scalars so a
    /// deferred one cannot name freed memory. The array is snapshotted when the
    /// signal fires, and this reads that snapshot back.
    ///
    /// So the honest description of what this returns is **the most recent set
    /// wlroots reported**, which for a handler called synchronously is the
    /// current one, and for a deferred delivery may be a later one than the
    /// event that woke it named. A set that has been superseded is the only
    /// thing a deferred event could honestly report; the alternative is a
    /// snapshot of a state that no longer holds.
    ///
    /// `None` when nothing has been reported for `node` — it is not observed,
    /// no update has fired yet, or the run that was observing it has ended. An
    /// empty `Vec` is different, and means the node is displayed nowhere.
    /// `None` also when the last emission named a scene output this crate
    /// could not resolve — briefly true at hotplug, because wlroots updates a
    /// node's output set from inside `wlr_scene_output_create`, before this
    /// crate has given the new output an id. A shortened list is not a smaller
    /// truth: it is the same shape as a correct answer with a monitor missing
    /// from it, so the snapshot is dropped instead and the next emission
    /// refreshes it.
    pub fn scene_buffer_active_outputs(&self, node: NodeId) -> Option<Vec<SceneOutputId>> {
        self.inner.scene_buffer_outputs.borrow().get(&node).cloned()
    }

    /// Record what an `outputs_update` emission reported. Called from
    /// `backend.rs` at emission time.
    pub(crate) fn record_scene_buffer_outputs(&self, node: NodeId, active: Vec<SceneOutputId>) {
        // `try_borrow_mut`: this is reached from an `extern "C"` frame, where a
        // panic is an abort. No borrow of this table is ever held across a call
        // into wlroots, so it cannot fail; if it ever did, the snapshot is
        // simply not refreshed.
        if let Ok(mut table) = self.inner.scene_buffer_outputs.try_borrow_mut() {
            table.insert(node, active);
        }
    }

    /// Drop `node`'s snapshot. Called when it is destroyed or unobserved.
    pub(crate) fn forget_scene_buffer_outputs(&self, node: NodeId) {
        // `try_borrow_mut` for the reason `record_scene_buffer_outputs` gives:
        // one caller is a destroy callback under an `extern "C"` frame.
        if let Ok(mut table) = self.inner.scene_buffer_outputs.try_borrow_mut() {
            table.remove(&node);
        }
    }

    // ---------------------------------------------------------------------
    // Scene outputs (0.20.19)
    //
    // A scene output is the viewport that turns the scene into pixels on one
    // `wlr_output`. Unlike everything above, its id is backed by a destroy
    // listener rather than by an addon — `wlr_scene_output` has no addon set
    // for one to live in — and it therefore survives the `Backend::run_all`
    // call that created it, because the listener does. See `scene/output.rs`.
    // ---------------------------------------------------------------------

    /// `id`'s scene output, or `None` if it has been destroyed.
    ///
    /// The liveness flag, not the row's presence, is the authority: the row
    /// removal in the destroy callback is best-effort, the flag is not.
    fn scene_output_ptr(&self, id: SceneOutputId) -> Option<NonNull<sys::wlr_scene_output>> {
        let table = self.inner.scene_outputs.borrow();
        let entry = table.get(&id)?;
        entry.alive.get().then_some(entry.raw)
    }

    /// The id this runtime already has for `raw`, if any.
    ///
    /// A linear scan, deliberately: a compositor has as many scene outputs as
    /// it has monitors, and a reverse table keyed by pointer would be a second
    /// thing for the destroy callback to keep in step for no measurable gain.
    pub(crate) fn scene_output_id_of(
        &self,
        raw: NonNull<sys::wlr_scene_output>,
    ) -> Option<SceneOutputId> {
        self.inner
            .scene_outputs
            .borrow()
            .iter()
            .find(|(_, entry)| entry.raw == raw && entry.alive.get())
            .map(|(id, _)| *id)
    }

    /// Give `raw` an id and start watching it for destruction.
    ///
    /// Idempotent: a scene output this runtime already knows keeps the id it
    /// has, which matters because a second watch would purge the row twice.
    ///
    /// # Safety
    ///
    /// `raw` must point at a live `wlr_scene_output` belonging to this
    /// runtime's scene.
    pub(crate) unsafe fn record_scene_output(
        &self,
        raw: NonNull<sys::wlr_scene_output>,
    ) -> SceneOutputId {
        if let Some(id) = self.scene_output_id_of(raw) {
            return id;
        }
        let id = SceneOutputId(next_id());
        // SAFETY: forwarded from this function's own contract, plus the lookup
        // above, which established this runtime does not already watch `raw`.
        let entry = unsafe { crate::scene::output::watch(raw, &self.inner, id) };
        self.inner.scene_outputs.borrow_mut().insert(id, entry);
        id
    }

    /// The scene output showing this runtime's scene on `output`.
    ///
    /// `None` before [`init_graphics`](Runtime::init_graphics) has run, for an
    /// unknown or stale [`OutputId`], and for an output that has never been
    /// added to the scene — [`init_output`](Runtime::init_output) is what adds
    /// one, and [`add_scene_output`](Runtime::add_scene_output) is the way to
    /// add an output that should not go in the layout.
    ///
    /// An output added to the scene by this crate already has an id; one added
    /// through the raw pointers gets one here.
    pub fn scene_output(&self, output: OutputId) -> Option<SceneOutputId> {
        let raw = self.output_ptr(output)?;
        let scene = self.scene_ptr()?;
        // SAFETY: a present `outputs` entry names a live output (removed by
        // `forget_output` before wlroots frees it), and the scene is this
        // runtime's own. `wlr_scene_get_scene_output` returns null for an
        // output the scene does not have, which is checked.
        let so = unsafe { sys::wlr_scene_get_scene_output(scene.as_ptr(), raw.as_ptr()) };
        let so = NonNull::new(so)?;
        // SAFETY: wlroots returned a scene output of this runtime's own scene,
        // so it is live.
        Some(unsafe { self.record_scene_output(so) })
    }

    /// Add `output` to the scene without putting it in the output layout.
    ///
    /// [`init_output`](Runtime::init_output) is what a compositor normally
    /// calls — it initialises the renderer, places the output in the layout and
    /// adds it here, all of which an output that should actually be displayed
    /// needs. This is the narrower call for an output whose position the
    /// consumer intends to drive themselves with
    /// [`set_scene_output_position`](Runtime::set_scene_output_position).
    ///
    /// `None`, having added nothing, before
    /// [`init_graphics`](Runtime::init_graphics) has run, for an unknown or
    /// stale id, **or when the output is already in the scene** —
    /// `wlr_scene_output_create`'s own documentation is that an output can be
    /// added only once, and wlroots asserts it, which on this distribution's
    /// build is a process abort rather than an error. Use
    /// [`scene_output`](Runtime::scene_output) to ask for the existing one.
    pub fn add_scene_output(&self, output: OutputId) -> Option<SceneOutputId> {
        let raw = self.output_ptr(output)?;
        let scene = self.scene_ptr()?;
        // SAFETY: as in `scene_output`.
        let existing = unsafe { sys::wlr_scene_get_scene_output(scene.as_ptr(), raw.as_ptr()) };
        if !existing.is_null() {
            return None;
        }
        // SAFETY: as above; the check just made is `wlr_scene_output_create`'s
        // own "only once" precondition.
        let so = unsafe { sys::wlr_scene_output_create(scene.as_ptr(), raw.as_ptr()) };
        let so = NonNull::new(so)?;
        // SAFETY: wlroots just created this scene output in this runtime's
        // scene, so it is live and unwatched.
        Some(unsafe { self.record_scene_output(so) })
    }

    /// Destroy a scene output, so the scene stops rendering to that output.
    ///
    /// The `wlr_output` itself is untouched — this removes the viewport, not
    /// the monitor. Every [`SceneOutputId`] naming it misses cleanly
    /// afterwards, including this one.
    ///
    /// `None`, having destroyed nothing, for an unknown or already-destroyed
    /// id — a double destroy misses rather than double-freeing.
    pub fn destroy_scene_output(&self, scene_output: SceneOutputId) -> Option<()> {
        // Refused while any scene-node or scene-output borrow is live, for the
        // reason `remove_rect` documents: the handle held by the closure that
        // is calling us would dangle for the rest of that closure, and a
        // `scene_output_for_each_buffer` walk would read a freed list link.
        if self.scene_is_being_walked() {
            return None;
        }
        let raw = self.scene_output_ptr(scene_output)?;
        // SAFETY: a resolvable id names a scene output wlroots has not freed —
        // its destroy listener would have cleared the liveness flag otherwise.
        // The row is dropped from the inside, by that listener, during this
        // call, which is also what unlinks the listener itself.
        unsafe { sys::wlr_scene_output_destroy(raw.as_ptr()) };
        Some(())
    }

    /// Place a scene output's viewport at `(lx, ly)` in layout coordinates.
    ///
    /// For an output [`init_output`](Runtime::init_output) placed, the scene
    /// output layout already keeps this in step with the output layout, and
    /// [`set_output_position`](Runtime::set_output_position) is the call that
    /// moves both together. This is the lower-level one, for an output added
    /// with [`add_scene_output`](Runtime::add_scene_output).
    ///
    /// `None` for an unknown or destroyed id.
    pub fn set_scene_output_position(
        &self,
        scene_output: SceneOutputId,
        lx: i32,
        ly: i32,
    ) -> Option<()> {
        let raw = self.scene_output_ptr(scene_output)?;
        // SAFETY: a resolvable id names a live scene output.
        unsafe { sys::wlr_scene_output_set_position(raw.as_ptr(), lx, ly) };
        Some(())
    }

    /// A scene output's viewport position in layout coordinates. `None` for an
    /// unknown or destroyed id.
    pub fn scene_output_position(&self, scene_output: SceneOutputId) -> Option<(i32, i32)> {
        let raw = self.scene_output_ptr(scene_output)?;
        // SAFETY: a resolvable id names a live scene output.
        Some(unsafe { ((*raw.as_ptr()).x, (*raw.as_ptr()).y) })
    }

    /// Whether the scene has anything new to draw on this output.
    ///
    /// `false` means [`commit_scene_output`](Runtime::commit_scene_output)
    /// would skip, and a compositor pacing itself off frame events can skip
    /// too. `None` for an unknown or destroyed id.
    pub fn scene_output_needs_frame(&self, scene_output: SceneOutputId) -> Option<bool> {
        let raw = self.scene_output_ptr(scene_output)?;
        // SAFETY: a resolvable id names a live scene output.
        Some(unsafe { sys::wlr_scene_output_needs_frame(raw.as_ptr()) })
    }

    /// Render and present this scene output, with options.
    ///
    /// The lower-level half of [`commit_output`](Runtime::commit_output):
    /// that one is the whole body of a `frame` handler (commit, then
    /// frame-done), while this one commits and nothing else, and takes the
    /// timer, colour transform and swapchain
    /// [`SceneOutputStateOptions`](crate::SceneOutputStateOptions) carries.
    ///
    /// `Ok(true)` means a frame was rendered and presented. `Ok(false)` means
    /// wlroots legitimately skipped, because nothing had changed since the last
    /// one — not a failure, and the case a compositor rendering on a timer will
    /// see most often. The distinction is not in the C return value, which is
    /// `true` for both; it comes from asking
    /// [`scene_output_needs_frame`](Runtime::scene_output_needs_frame)
    /// immediately before, which is exactly the test
    /// `wlr_scene_output_commit` makes for itself (verified by disassembling
    /// `libwlroots-0.20.so`: the commit's first act is that call, returning
    /// `true` at once when it is false).
    ///
    /// # Errors
    ///
    /// [`Error::Destroyed`] for an unknown or destroyed id.
    /// [`Error::Operation`] if wlroots rejected the commit — a genuine failure,
    /// which for these options usually means a swapchain whose dimensions do
    /// not match the output, or a colour transform on an output that already
    /// has an image description. It is also what a commit attempted while any
    /// mapping opened by
    /// [`Buffer::begin_data_ptr_access`](crate::Buffer::begin_data_ptr_access)
    /// is live on this thread returns, for the reason
    /// [`commit_output`](Runtime::commit_output) gives.
    pub fn commit_scene_output(
        &self,
        scene_output: SceneOutputId,
        options: &SceneOutputStateOptions<'_>,
    ) -> Result<bool> {
        if crate::buffer::any_data_ptr_access_open() {
            return Err(Error::Operation("Runtime::commit_scene_output"));
        }
        let Some(raw) = self.scene_output_ptr(scene_output) else {
            return Err(Error::Destroyed("wlr_scene_output"));
        };
        let opts = options.as_c();
        // SAFETY: a resolvable id names a live scene output; `opts` borrows the
        // timer, transform and swapchain from `options`, all of which outlive
        // this call.
        unsafe {
            let needed = sys::wlr_scene_output_needs_frame(raw.as_ptr());
            if !sys::wlr_scene_output_commit(raw.as_ptr(), &raw const opts) {
                return Err(Error::Operation("wlr_scene_output_commit"));
            }
            Ok(needed)
        }
    }

    /// Tell every surface this scene output rendered that it may draw again.
    ///
    /// `when` is the presentation timestamp handed to the clients, as a
    /// duration since whatever epoch the compositor's clock uses.
    /// [`commit_output`](Runtime::commit_output) does this itself with the
    /// current time; this is the call for a compositor driving the two halves
    /// separately. `None` for an unknown or destroyed id.
    pub fn send_scene_output_frame_done(
        &self,
        scene_output: SceneOutputId,
        when: std::time::Duration,
    ) -> Option<()> {
        let raw = self.scene_output_ptr(scene_output)?;
        let mut now = timespec_of(when);
        // SAFETY: a resolvable id names a live scene output, and `now` is a
        // live local for the call.
        unsafe { sys::wlr_scene_output_send_frame_done(raw.as_ptr(), &raw mut now) };
        Some(())
    }

    /// Visit every buffer node **visible on this output**, in render order.
    ///
    /// The scene-output half of
    /// [`for_each_buffer`](Runtime::for_each_buffer): same root-to-leaves
    /// order, same layout-local positions, and the same rule that nothing may
    /// free or move a node from inside `f` — every call that could refuses
    /// while the walk is running. The difference is the filter: this visits
    /// what is actually on screen for this output, so a node scrolled off it,
    /// or on another monitor, is not visited.
    ///
    /// `None` for an unknown or destroyed id.
    ///
    /// # Panics
    ///
    /// As for [`for_each_buffer`](Runtime::for_each_buffer): a panic escaping
    /// `f` is caught, the remaining nodes are skipped, and it is resumed once
    /// wlroots' own iteration has returned.
    pub fn scene_output_for_each_buffer(
        &self,
        scene_output: SceneOutputId,
        f: impl FnMut(NodeId, i32, i32),
    ) -> Option<()> {
        let raw = self.scene_output_ptr(scene_output)?;
        self.buffer_walk(f, |iterator, user_data| {
            // SAFETY: a resolvable id names a live scene output; the iterator
            // has `wlr_scene_buffer_iterator_func_t`'s signature and
            // `user_data` outlives the call, both guaranteed by `buffer_walk`.
            unsafe { sys::wlr_scene_output_for_each_buffer(raw.as_ptr(), iterator, user_data) };
        });
        Some(())
    }

    /// Borrow a scene output as a [`SceneOutput`] handle for the duration of
    /// `f`.
    ///
    /// The handle cannot escape `f`, and neither can the
    /// [`DamageRingRef`](crate::DamageRingRef) reached through it — which is
    /// the point: that ring is embedded in the scene output and dies with it.
    ///
    /// `None`, without calling `f`, for an unknown or destroyed id.
    pub fn with_scene_output<R>(
        &self,
        scene_output: SceneOutputId,
        f: impl FnOnce(&SceneOutput<'_>) -> R,
    ) -> Option<R> {
        let raw = self.scene_output_ptr(scene_output)?;
        // Raised before the handle is minted and lowered by `Drop`, so a panic
        // escaping `f` cannot leave the runtime permanently refusing destroys.
        let _guard = NodeBorrowGuard::enter(&self.inner);
        // The borrow guard only makes *this crate's* by-id destroys refuse. It
        // says nothing to wlroots, which frees nodes on its own schedule the
        // moment the event loop runs — and the closure can drive that loop, by
        // holding a `&Display` or an `&EventLoop`. A client unmapping its
        // window mid-closure would then free the subtree under a live handle.
        // `ForeignFrame` sets the same flag a real handler delivery does, so
        // `EventLoop::dispatch` refuses for the life of the borrow and that
        // window cannot open.
        let _frame = crate::dispatch::ForeignFrame::enter();
        // SAFETY: a resolvable id names a live scene output, and the guard
        // above is what keeps it live for the whole of `f`.
        //
        // Reasoning about what `SceneOutput`'s own methods can do is not
        // enough, and was the bug here: `f` captures the `Runtime` it was
        // called on, so it can call `destroy_scene_output` directly — the
        // hazard arrives through the closure's captures, not through the
        // handle. `destroy_scene_output` consults this guard for that reason.
        let handle = unsafe { SceneOutput::from_raw_with_id(raw.as_ptr(), scene_output) };
        Some(f(&handle))
    }

    // ---------------------------------------------------------------------
    // Scene surfaces and the helpers built on them (0.20.19)
    // ---------------------------------------------------------------------

    /// Borrow `node` as a [`SceneSurface`] handle, if it is a buffer node whose
    /// pixels come from a client surface.
    ///
    /// The [`with_node`](Runtime::with_node) contract applies verbatim,
    /// including the destroy refusals while the borrow is live. `None`,
    /// without calling `f`, when the id is unknown, stale, not a buffer node,
    /// or a buffer node this crate uploaded pixels into rather than a client's.
    ///
    /// This is how a hit test becomes a surface: feed it the [`NodeId`]
    /// [`node_at`](Runtime::node_at) returned. `wlr_scene_surface_try_from_buffer`
    /// answers with null rather than misbehaving on a node that is not one, so
    /// asking is always safe.
    pub fn with_scene_surface<R>(
        &self,
        node: NodeId,
        f: impl FnOnce(&SceneSurface<'_>) -> R,
    ) -> Option<R> {
        let raw = self.scene_surface_ptr(node)?;
        let _guard = NodeBorrowGuard::enter(&self.inner);
        // The borrow guard only makes *this crate's* by-id destroys refuse. It
        // says nothing to wlroots, which frees nodes on its own schedule the
        // moment the event loop runs — and the closure can drive that loop, by
        // holding a `&Display` or an `&EventLoop`. A client unmapping its
        // window mid-closure would then free the subtree under a live handle.
        // `ForeignFrame` sets the same flag a real handler delivery does, so
        // `EventLoop::dispatch` refuses for the life of the borrow and that
        // window cannot open.
        let _frame = crate::dispatch::ForeignFrame::enter();
        // SAFETY: `scene_surface_ptr` resolved the node and asked wlroots
        // whether it is surface-backed, so `raw` is a live `wlr_scene_surface`;
        // the guard is what keeps its buffer node alive for the whole of `f`.
        let handle = unsafe { SceneSurface::from_raw_with_node(raw.as_ptr(), node) };
        Some(f(&handle))
    }

    /// `node`'s scene surface, if it has one.
    ///
    /// The shared resolution step: tag-check the node, downcast it to a buffer
    /// node, then ask wlroots whether that buffer is surface-backed.
    fn scene_surface_ptr(&self, node: NodeId) -> Option<NonNull<sys::wlr_scene_surface>> {
        let raw = self.node_ptr(node)?;
        // SAFETY: a resolvable id names a live node.
        if NodeKind::from_raw(unsafe { (*raw.as_ptr()).type_ }) != Some(NodeKind::Buffer) {
            return None;
        }
        // SAFETY: the tag check above is `wlr_scene_buffer_from_node`'s whole
        // precondition, and `wlr_scene_surface_try_from_buffer` reports "not a
        // surface" as null rather than as undefined behaviour.
        unsafe {
            let buffer = sys::wlr_scene_buffer_from_node(raw.as_ptr());
            NonNull::new(sys::wlr_scene_surface_try_from_buffer(buffer))
        }
    }

    /// Tell one surface it may draw again, if it is visible.
    ///
    /// [`send_scene_output_frame_done`](Runtime::send_scene_output_frame_done)
    /// is the usual call — it covers every surface an output rendered. This one
    /// is for a compositor answering a single surface, and wlroots skips it
    /// silently when that surface is not actually on screen.
    ///
    /// `None` when the id is unknown, stale, or not a surface-backed buffer
    /// node.
    pub fn send_scene_surface_frame_done(
        &self,
        node: NodeId,
        when: std::time::Duration,
    ) -> Option<()> {
        let raw = self.scene_surface_ptr(node)?;
        let now = timespec_of(when);
        // SAFETY: a resolvable surface-backed node names a live scene surface,
        // and `now` is a live local wlroots reads out of.
        unsafe { sys::wlr_scene_surface_send_frame_done(raw.as_ptr(), &raw const now) };
        Some(())
    }

    /// Fire one buffer node's own `frame_done` signal.
    ///
    /// The signal a consumer watching a scene buffer
    /// ([`observe_scene_buffer`](Runtime::observe_scene_buffer)) receives as
    /// [`OutputHandler::scene_buffer_frame_done`](crate::OutputHandler::scene_buffer_frame_done).
    /// Unlike [`send_scene_surface_frame_done`](Runtime::send_scene_surface_frame_done)
    /// this sends the client nothing — it is the scene's own notification, for
    /// a buffer node whose pixels a compositor produces itself.
    ///
    /// `None` when either id is unknown or stale, or when `node` is not a
    /// buffer node.
    pub fn send_scene_buffer_frame_done(
        &self,
        node: NodeId,
        scene_output: SceneOutputId,
        when: std::time::Duration,
    ) -> Option<()> {
        let raw = self.restylable_scene_buffer_ptr(node)?;
        let output = self.scene_output_ptr(scene_output)?;
        let mut event = sys::wlr_scene_frame_done_event {
            output: output.as_ptr(),
            when: timespec_of(when),
        };
        // SAFETY: both ids resolve, so both name live objects, and `event` is a
        // live local wlroots reads out of and passes to its listeners.
        unsafe { sys::wlr_scene_buffer_send_frame_done(raw.as_ptr(), &raw mut event) };
        Some(())
    }

    /// Crop every subsurface tree beneath `node` to `clip`.
    ///
    /// The clip is in the coordinate space of the **root surface** of each
    /// subsurface tree, not of `node`, and `None` (or an empty box) disables
    /// clipping. This is what makes a window that is larger than the space a
    /// compositor wants to give it render cropped rather than overflowing —
    /// the scene applies it to the client's subsurfaces as well as to its main
    /// surface, which hand-positioning each node cannot do.
    ///
    /// `None` for an unknown or stale id. A node with no subsurface tree under
    /// it is not an error: the call simply has nothing to clip.
    pub fn set_subsurface_tree_clip(&self, node: NodeId, clip: Option<Box2D>) -> Option<()> {
        let raw = self.node_ptr(node)?;
        let ptr = clip.as_ref().map_or(std::ptr::null(), Box2D::as_c);
        // SAFETY: a resolvable id names a live node; `ptr` is null or points at
        // `clip`, a live local whose layout is pinned to `wlr_box`, which
        // wlroots copies out of.
        unsafe { sys::wlr_scene_subsurface_tree_set_clip(raw.as_ptr(), ptr) };
        Some(())
    }

    /// Position and configure a layer surface from its own anchoring state, and
    /// subtract what it claims from `usable`.
    ///
    /// This is wlroots' own layer-shell arithmetic, which
    /// [`configure_layer_surface`](Runtime::configure_layer_surface) (a raw
    /// "tell the client this size") deliberately does not do: it reads the
    /// surface's anchors, margins and exclusive zone, moves its scene node,
    /// sends the configure, and **mutates `usable` in place** so the next layer
    /// surface sees what is left. Thread one `usable` through every layer
    /// surface of an output, in order, starting from the output's own box:
    ///
    /// ```ignore
    /// let full = Box2D::new(0, 0, width, height);
    /// let mut usable = full;
    /// for id in my_layer_surfaces_in_order {
    ///     runtime.configure_scene_layer_surface(id, full, &mut usable);
    /// }
    /// ```
    ///
    /// `None`, having configured nothing and left `usable` untouched, when the
    /// id is unknown or stale, or when the surface is **not yet initialized** —
    /// wlroots' `wlr_layer_surface_v1_configure`, which this reaches, asserts
    /// `initialized`, and this distribution's build of wlroots turns that into
    /// a process abort. Unlike
    /// [`configure_layer_surface`](Runtime::configure_layer_surface) there is
    /// nothing to stage for later here: the call also moves the scene node and
    /// rewrites `usable`, neither of which can be replayed at the surface's
    /// next commit without the caller's boxes. Call this from
    /// [`ToplevelHandler::layer_surface_commit`](crate::ToplevelHandler::layer_surface_commit),
    /// which runs after that first commit, rather than from
    /// `new_layer_surface`.
    pub fn configure_scene_layer_surface(
        &self,
        id: LayerSurfaceId,
        full: Box2D,
        usable: &mut Box2D,
    ) -> Option<()> {
        let (raw, scene) = {
            let table = self.inner.layer_surfaces.borrow();
            let entry = table.get(&id)?;
            (entry.raw, entry.scene)
        };
        // SAFETY: an entry is removed by `on_layer_surface_destroy` before
        // wlroots frees the layer surface, so a present entry names a live one.
        if !unsafe { (*raw.as_ptr()).initialized } {
            return None;
        }
        // SAFETY: as above for `scene`, which wlroots frees together with the
        // layer surface. `full` is read and `usable` is written through, both
        // live locals of the caller's whose layout is pinned to `wlr_box`, and
        // the `initialized` check above is the assert this call would otherwise
        // abort on.
        unsafe {
            sys::wlr_scene_layer_surface_v1_configure(
                scene.as_ptr(),
                full.as_c(),
                (&raw mut *usable).cast::<sys::wlr_box>(),
            );
        }
        Some(())
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
        let raw = NonNull::new(raw).ok_or(Error::Create(
            "wlr_primary_selection_v1_device_manager_create",
        ))?;
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

    /// Create the `zwlr_screencopy_manager_v1` global, letting clients capture
    /// an output's rendered contents (grim, wf-recorder, screen-sharing
    /// portals). wlroots implements the whole capture flow — buffer
    /// negotiation, the copy, damage, and the `ready`/`failed` result — so
    /// there is nothing further to wire. Errors if called twice.
    pub fn create_screencopy_manager(&self, display: &Display) -> Result<()> {
        if self.inner.screencopy_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_screencopy_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_screencopy_manager_v1_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_screencopy_manager_v1_create"))?;
        *self.inner.screencopy_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Create the `zwp_pointer_constraints_v1` global, letting clients confine
    /// or lock the pointer to a region of a surface. Errors if called twice.
    pub fn create_pointer_constraints_manager(&self, display: &Display) -> Result<()> {
        if self.inner.pointer_constraints_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_pointer_constraints_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_pointer_constraints_v1_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_pointer_constraints_v1_create"))?;
        *self.inner.pointer_constraints_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The `zwp_pointer_constraints_v1` manager, once created via
    /// [`Runtime::create_pointer_constraints_manager`] — read by `backend.rs`'s
    /// `register_toplevel_and_input` to link the `new_constraint` listener.
    pub(crate) fn pointer_constraints_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_pointer_constraints_v1>> {
        *self.inner.pointer_constraints_manager.borrow()
    }

    /// Create the `wp_cursor_shape_manager_v1` global, letting clients name
    /// the cursor image they want instead of drawing their own. Errors if
    /// called twice.
    ///
    /// wlroots does not apply the request itself — its own doc on
    /// `wlr_cursor_shape_manager_v1` says a compositor should handle the
    /// `request_set_shape` event "in the same way as
    /// `wlr_seat.events.request_set_cursor`" — so this only advertises the
    /// global and wires each request through to
    /// [`crate::SeatHandler::request_set_shape`]; applying it is
    /// [`Runtime::set_cursor_shape`], called from that handler.
    pub fn create_cursor_shape_manager(&self, display: &Display) -> Result<()> {
        if self.inner.cursor_shape_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_cursor_shape_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        // `2` is the newest version this build's headers support —
        // `wp_cursor_shape_device_v1_shape`'s `DND_ASK`/`ALL_RESIZE` variants
        // were both added in cursor-shape-v1 version 2, and both are present
        // in the bound enum (see `CursorShape::from_raw`).
        let raw = unsafe { sys::wlr_cursor_shape_manager_v1_create(display.as_ptr(), 2) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_cursor_shape_manager_v1_create"))?;
        *self.inner.cursor_shape_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The `wp_cursor_shape_manager_v1` manager, once created via
    /// [`Runtime::create_cursor_shape_manager`] — read by `backend.rs`'s
    /// `register_toplevel_and_input` to link the `request_set_shape` listener.
    pub(crate) fn cursor_shape_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_cursor_shape_manager_v1>> {
        *self.inner.cursor_shape_manager.borrow()
    }

    /// Create the `xdg_activation_v1` global, letting a client request that
    /// one of its surfaces be given focus. Errors if called twice.
    ///
    /// wlroots validates every request before it reaches
    /// [`crate::SeatHandler::request_activate`] — see [`crate::ActivationToken`]'s
    /// own doc — so that handler only ever sees issuance wlroots itself
    /// accepted. Applying (or refusing) the activation is entirely the
    /// compositor's own focus-steal policy; this crate neither steals focus
    /// nor blocks the request on its own.
    pub fn create_xdg_activation_manager(&self, display: &Display) -> Result<()> {
        if self.inner.xdg_activation_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_xdg_activation_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        // `wlr_xdg_activation_v1_create` takes no version argument — the
        // protocol has had exactly one interface version since it was
        // introduced.
        let raw = unsafe { sys::wlr_xdg_activation_v1_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_xdg_activation_v1_create"))?;
        *self.inner.xdg_activation_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The `xdg_activation_v1` manager, once created via
    /// [`Runtime::create_xdg_activation_manager`] — read by `backend.rs`'s
    /// `register_toplevel_and_input` to link the `request_activate` listener.
    pub(crate) fn xdg_activation_manager_ptr(&self) -> Option<NonNull<sys::wlr_xdg_activation_v1>> {
        *self.inner.xdg_activation_manager.borrow()
    }

    /// Create the `zwlr_gamma_control_manager_v1` global, letting a client
    /// (a night-light tool such as `wlsunset` or `gammastep`) set a per-output
    /// gamma ramp. Errors if called twice, or if [`Runtime::init_graphics`]
    /// has not run yet — there is no scene to wire the manager into before
    /// that.
    ///
    /// Wired straight into this runtime's scene via
    /// `wlr_scene_set_gamma_control_manager_v1` — wlroots' own header
    /// documents this as *the* way to handle `gamma_control_v1` for a scene
    /// ("Handles gamma_control_v1 for all outputs in the scene"), and this
    /// crate always renders through a scene. Nothing here reimplements the
    /// apply/fail/destroy dance by hand: this wlroots build exposes no public
    /// `wlr_output_state` gamma-LUT setter to reimplement it *with* — only
    /// `wlr_gamma_control_v1_apply(control, output_state)`, which the scene
    /// integration already calls on the compositor's behalf during its own
    /// commit. A compositor that wants to know when an output's gamma
    /// changed overrides [`crate::OutputHandler::gamma_control_changed`]; it
    /// is a notification alongside the scene's automatic apply, not a hook
    /// this crate expects it to drive the apply through.
    pub fn create_gamma_control_manager(&self, display: &Display) -> Result<()> {
        if self.inner.gamma_control_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_gamma_control_manager called twice",
            ));
        }
        let scene = self.scene_ptr().ok_or(Error::Operation(
            "Runtime::create_gamma_control_manager called before Runtime::init_graphics",
        ))?;
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_gamma_control_manager_v1_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_gamma_control_manager_v1_create"))?;
        // SAFETY: `scene` is this runtime's own live scene, created by
        // `init_graphics` and never freed while `self` lives; `raw` was just
        // null-checked and is owned by `display`, which outlives `scene`
        // (both are freed when `display` is). `wlr_scene_set_gamma_control_manager_v1`
        // asserts a scene has no manager set yet — the double-create guard
        // above is what makes that true, since this is the only call site.
        unsafe {
            sys::wlr_scene_set_gamma_control_manager_v1(scene.as_ptr(), raw.as_ptr());
        }
        *self.inner.gamma_control_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The `zwlr_gamma_control_manager_v1` manager, once created via
    /// [`Runtime::create_gamma_control_manager`] — read by `backend.rs`'s
    /// `register_toplevel_and_input` to link the `set_gamma` notification
    /// listener.
    pub(crate) fn gamma_control_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_gamma_control_manager_v1>> {
        *self.inner.gamma_control_manager.borrow()
    }

    /// Create the `zwlr_output_manager_v1` global, letting clients enumerate
    /// output heads and request an atomic reconfiguration. Errors if called
    /// twice.
    pub fn create_output_manager(&self, display: &Display) -> Result<()> {
        if self.inner.output_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_output_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_output_manager_v1_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_output_manager_v1_create"))?;
        *self.inner.output_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The `zwlr_output_manager_v1` manager, once created via
    /// [`Runtime::create_output_manager`] — read by `backend.rs`'s
    /// manager-setup block to link the `apply`/`test` listeners, and by
    /// [`update_output_manager_state`](Runtime::update_output_manager_state).
    pub(crate) fn output_manager_ptr(&self) -> Option<NonNull<sys::wlr_output_manager_v1>> {
        *self.inner.output_manager.borrow()
    }

    /// Broadcast the compositor's current output layout to
    /// `zwlr_output_manager_v1` clients.
    ///
    /// Builds a fresh `wlr_output_configuration_v1` describing every output
    /// this runtime knows of — one config head per output, pre-filled by
    /// wlroots from the output's committed state (enabled/mode/scale/transform)
    /// and given its layout position from
    /// [`output_layout_box`](Runtime::output_layout_box) — then hands it to
    /// `wlr_output_manager_v1_set_configuration`, which takes ownership and
    /// sends the current state to bound clients.
    ///
    /// A no-op when no manager has been created (a compositor that never called
    /// [`create_output_manager`](Runtime::create_output_manager)). Compositors
    /// call this after any output change — hotplug, or applying a persisted
    /// layout — so clients see an up-to-date view.
    pub fn update_output_manager_state(&self) {
        let Some(manager) = self.output_manager_ptr() else {
            return;
        };

        // Snapshot the (id, raw) pairs before any wlroots call below, so no
        // `outputs` borrow is held across FFI, matching every other method
        // here. `output_layout_box` borrows `graphics`, not `outputs`, so
        // querying positions after the snapshot is borrow-safe.
        let outputs: Vec<(OutputId, NonNull<sys::wlr_output>)> = self
            .inner
            .outputs
            .borrow()
            .iter()
            .map(|(id, raw)| (*id, *raw))
            .collect();

        // SAFETY: `wlr_output_configuration_v1_create` allocates a fresh,
        // owned configuration. Each recorded `raw` names an output still live
        // in this run — `forget_output` removes an entry synchronously before
        // wlroots frees the output — so passing it to
        // `wlr_output_configuration_head_v1_create` (which reads the output and
        // links a head into the config) is sound. The head it returns is owned
        // by the config; writing its `state.x`/`.y` is a plain field
        // assignment. `set_configuration` takes ownership of the config and
        // frees it, so this crate never touches it again.
        unsafe {
            let config = sys::wlr_output_configuration_v1_create();
            if config.is_null() {
                return;
            }
            for (id, raw) in outputs {
                let head = sys::wlr_output_configuration_head_v1_create(config, raw.as_ptr());
                if head.is_null() {
                    continue;
                }
                // `head_v1_create` pre-fills mode/scale/transform/enabled but
                // not position; supply it from the layout box.
                if let Some((x, y, _, _)) = self.output_layout_box(id) {
                    (*head).state.x = x;
                    (*head).state.y = y;
                }
            }
            sys::wlr_output_manager_v1_set_configuration(manager.as_ptr(), config);
        }
    }

    /// Create the `wp_viewporter` global. Clients crop/scale their buffers
    /// via a viewport; wlroots' scene applies it at render time, so no
    /// handler is needed. Errors if called twice.
    pub fn create_viewporter(&self, display: &Display) -> Result<()> {
        if self.inner.viewporter.borrow().is_some() {
            return Err(Error::Operation("Runtime::create_viewporter called twice"));
        }
        // SAFETY: `display` is live for the call; the returned viewporter is
        // owned by the display and destroyed with it, so this crate never
        // frees it.
        let raw = unsafe { sys::wlr_viewporter_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_viewporter_create"))?;
        *self.inner.viewporter.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Create the `wp_single_pixel_buffer_manager_v1` global, letting a
    /// client create a cheap solid-colour buffer without a shm/dmabuf pool.
    /// The renderer consumes the buffer type automatically. Errors if called
    /// twice.
    pub fn create_single_pixel_buffer_manager(&self, display: &Display) -> Result<()> {
        if self.inner.single_pixel_buffer_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_single_pixel_buffer_manager called twice",
            ));
        }
        // SAFETY: as `create_viewporter`.
        let raw = unsafe { sys::wlr_single_pixel_buffer_manager_v1_create(display.as_ptr()) };
        let raw =
            NonNull::new(raw).ok_or(Error::Create("wlr_single_pixel_buffer_manager_v1_create"))?;
        *self.inner.single_pixel_buffer_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Create the `wp_content_type_manager_v1` global, at protocol version 1
    /// (the only version this wlroots supports). Content-type is surface
    /// metadata wlroots attaches; no handler is required to make use of it.
    /// Errors if called twice.
    pub fn create_content_type_manager(&self, display: &Display) -> Result<()> {
        if self.inner.content_type_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_content_type_manager called twice",
            ));
        }
        // SAFETY: as `create_viewporter`.
        let raw = unsafe { sys::wlr_content_type_manager_v1_create(display.as_ptr(), 1) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_content_type_manager_v1_create"))?;
        *self.inner.content_type_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Create the `zxdg_output_manager_v1` global — read-only per-output
    /// logical geometry that many panels and toolkits read. Distinct from
    /// this crate's `zwlr_output_manager_v1` wrapper
    /// ([`create_output_manager`](Runtime::create_output_manager)), which is
    /// the output-*management* (reconfiguration) protocol. Needs the scene's
    /// output layout, so call after
    /// [`init_graphics`](Runtime::init_graphics). Errors if called twice or
    /// before graphics init.
    pub fn create_xdg_output_manager(&self, display: &Display) -> Result<()> {
        if self.inner.xdg_output_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_xdg_output_manager called twice",
            ));
        }
        // Copied out and the borrow dropped before the wlroots call below,
        // matching every other graphics accessor here: nothing in this call
        // can re-enter this crate, but holding a `RefCell` borrow across an
        // FFI call this crate does not control is the one habit worth never
        // forming.
        let layout = self
            .inner
            .graphics
            .borrow()
            .as_ref()
            .map(|g| g.layout)
            .ok_or(Error::Operation(
                "Runtime::create_xdg_output_manager before init_graphics",
            ))?;
        // SAFETY: `display` is live for the call; `layout` is this runtime's
        // own, created by `init_graphics` and never freed by this crate (see
        // [`Graphics`]'s own doc). The returned manager is owned by the
        // display and destroyed with it, so this crate never frees it.
        let raw =
            unsafe { sys::wlr_xdg_output_manager_v1_create(display.as_ptr(), layout.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_xdg_output_manager_v1_create"))?;
        *self.inner.xdg_output_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Create the `wp_fractional_scale_manager_v1` global, at protocol
    /// version 1 (the only version this wlroots supports). wlroots' scene
    /// sends each surface its preferred fractional scale on output-enter, so
    /// a client can render a sharp buffer for a fractional output scale
    /// (e.g. 1.5). Errors if called twice.
    pub fn create_fractional_scale_manager(&self, display: &Display) -> Result<()> {
        if self.inner.fractional_scale_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_fractional_scale_manager called twice",
            ));
        }
        // SAFETY: as `create_viewporter`.
        let raw = unsafe { sys::wlr_fractional_scale_manager_v1_create(display.as_ptr(), 1) };
        let raw =
            NonNull::new(raw).ok_or(Error::Create("wlr_fractional_scale_manager_v1_create"))?;
        *self.inner.fractional_scale_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Notify a client of `surface`'s current preferred fractional scale.
    ///
    /// wlroots' scene already sends this automatically for every surface it
    /// draws (`wlr_scene_surface_create`'s own documented behaviour — see
    /// [`SceneSurface`]'s module doc), so a compositor built entirely on the
    /// scene never needs to call this. It exists as a fallback for a surface
    /// this crate did not put in the scene, or for a compositor that wants to
    /// force an out-of-band update — harmless if called redundantly, since
    /// `wlr_fractional_scale_v1_notify_scale` only (re)sends the event to
    /// whatever `wp_fractional_scale_v1` object the client already bound, if
    /// any.
    pub fn notify_fractional_scale(&self, surface: &SceneSurface<'_>, scale: f64) {
        // SAFETY: `surface` borrows a live `wlr_scene_surface` for the
        // duration of this call (its own lifetime guarantees that), and its
        // `surface` field is the `wlr_surface` it wraps, non-null for as long
        // as the scene surface itself is live. `wlr_fractional_scale_v1_notify_scale`
        // only looks up the client's already-bound `wp_fractional_scale_v1`
        // resource (if any) and sends an event on it; it takes no ownership
        // and stores nothing past the call.
        unsafe {
            sys::wlr_fractional_scale_v1_notify_scale((*surface.as_ptr()).surface, scale);
        }
    }

    /// Create the `wp_presentation` global, letting a client request
    /// presentation feedback (when its buffer was actually presented, and at
    /// what refresh), at protocol version 2 (the max this wlroots supports).
    ///
    /// Unlike `zwp_linux_dmabuf_v1` feedback, which needs an explicit
    /// `wlr_scene_set_linux_dmabuf_v1` wiring call, wp_presentation_time has
    /// none in this wlroots version — `wlr_scene_surface_create`'s own
    /// documented behaviour lists it as one of the protocols a compositor
    /// "just needs to enable": every scene surface reports feedback
    /// automatically once this global exists. See
    /// [`set_scene_presentation`](Runtime::set_scene_presentation)'s own doc
    /// for the consequence. Errors if called twice.
    pub fn create_presentation(&self, display: &Display, backend: &Backend<'_>) -> Result<()> {
        if self.inner.presentation.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_presentation called twice",
            ));
        }
        // SAFETY: `display` and `backend` are live for the call; the returned
        // presentation global is owned by the display and destroyed with it,
        // so this crate never frees it.
        let raw = unsafe { sys::wlr_presentation_create(display.as_ptr(), backend.as_ptr(), 2) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_presentation_create"))?;
        *self.inner.presentation.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Confirm the scene is ready to report `wp_presentation` feedback.
    ///
    /// This wlroots version has **no** `wlr_scene_set_presentation` (or
    /// equivalent) wiring call to make — confirmed against both the
    /// installed `wlr_scene.h`/`wlr_presentation_time.h` headers and the
    /// generated bindings: `struct wlr_scene` carries no presentation field,
    /// and no such symbol exists to bind. wp_presentation_time is one of the
    /// protocols `wlr_scene_surface_create` documents as needing nothing
    /// beyond the global existing (see
    /// [`create_presentation`](Runtime::create_presentation)'s own doc), so
    /// there is no per-scene registration to perform. This method exists to
    /// enforce the ordering contract a consumer would otherwise have to track
    /// by hand — call after both
    /// [`init_graphics`](Runtime::init_graphics) and
    /// [`create_presentation`](Runtime::create_presentation) — and to give a
    /// stable name a future wlroots wiring requirement could grow real work
    /// under without an API break. Errors if either precondition is missing.
    pub fn set_scene_presentation(&self) -> Result<()> {
        if self.inner.presentation.borrow().is_none() {
            return Err(Error::Operation(
                "Runtime::set_scene_presentation before create_presentation",
            ));
        }
        if self.scene_ptr().is_none() {
            return Err(Error::Operation(
                "Runtime::set_scene_presentation before init_graphics",
            ));
        }
        Ok(())
    }

    /// The pointer constraint currently activated on the focused surface, or
    /// `None` when unconstrained. Read by the enforcement path (a follow-up
    /// task); cleared by `backend.rs`'s `on_pointer_constraint_destroy` when
    /// the active constraint is destroyed.
    pub(crate) fn active_constraint(&self) -> Option<NonNull<sys::wlr_pointer_constraint_v1>> {
        self.inner.active_constraint.get()
    }

    /// The implicit pointer grab in force, or `None` when no button is held.
    ///
    /// See [`PointerGrab`] for the model. Read by `backend.rs`'s motion and
    /// button paths, which are also its only writers.
    pub(crate) fn pointer_grab(&self) -> Option<PointerGrab> {
        self.inner.pointer_grab.get()
    }

    /// Install or clear the implicit pointer grab.
    pub(crate) fn set_pointer_grab(&self, grab: Option<PointerGrab>) {
        self.inner.pointer_grab.set(grab);
    }

    /// The cursor's current position in layout coordinates, or `(0.0, 0.0)`
    /// when no seat cursor exists (a consumer that never called
    /// [`create_seat`](Runtime::create_seat)). Read-only; the enforcement path
    /// and tests consult it to observe where the pointer is relative to any
    /// active constraint's region.
    ///
    /// A one-line delegate to [`pointer_position`](Runtime::pointer_position),
    /// the pre-existing accessor for the exact same value: keeping a single
    /// source of truth rather than two divergent bodies. The name is retained
    /// because tests and the `DbCommand` layer reference it.
    pub fn cursor_position(&self) -> (f64, f64) {
        self.pointer_position()
    }

    /// Whether the layout point `(x, y)` lies inside `region`, decided by
    /// wlroots' own predicate: [`wlr_region_confine`] returns `true` exactly
    /// when its *start* point is in the region (it `floor`s the start to
    /// integer pixels and asks `pixman_region32_contains_point`), so a
    /// zero-length confine from the point to itself is a containment test that
    /// uses the very same rounding the confine path uses. This is why the
    /// enforcement path can rely on it: the answer here matches what
    /// [`confine_motion`](Runtime::confine_motion) will decide for a motion
    /// starting at the same point. (The dedicated `pixman_region32_*`
    /// predicates are not exposed by `wlr-sys`'s `wlr_*`/`WLR_*` bindgen
    /// allowlist; `wlr_region_confine` is, and is the wlroots-consistent
    /// substitute.)
    ///
    /// # Safety
    ///
    /// `region` must point to a live `pixman_region32_t` (e.g. the `region`
    /// field of a live `wlr_pointer_constraint_v1`).
    pub(crate) unsafe fn region_contains_point(
        &self,
        region: *const sys::pixman_region32_t,
        x: f64,
        y: f64,
    ) -> bool {
        let mut ox = x;
        let mut oy = y;
        // SAFETY: `region` is live per the contract; the two out-params are
        // live stack locals. A start==end confine writes the start back into
        // the out-params on `true` and leaves them untouched on `false`; only
        // the bool is consulted.
        unsafe { sys::wlr_region_confine(region, x, y, x, y, &mut ox, &mut oy) }
    }

    /// Confine a motion from `(from_x, from_y)` to the intended `(to_x, to_y)`
    /// against `region`, returning the confined layout point when the *old*
    /// position `(from_x, from_y)` is inside the region, or `None` when it is
    /// not (the ray never entered the region). This is a thin, safe wrapper
    /// over [`wlr_region_confine`] that turns its `bool`/out-param contract
    /// into an `Option`: on `false` `wlr_region_confine` leaves the out-params
    /// untouched, so the caller must never read them — returning `None` makes
    /// that impossible to get wrong.
    ///
    /// # Safety
    ///
    /// `region` must point to a live `pixman_region32_t`.
    pub(crate) unsafe fn confine_motion(
        &self,
        region: *const sys::pixman_region32_t,
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
    ) -> Option<(f64, f64)> {
        // Initialised before the call so a spurious read can never see
        // uninitialised memory; on `false` they are simply discarded.
        let mut cx = to_x;
        let mut cy = to_y;
        // SAFETY: `region` is live per the contract; out-params are live locals.
        let ok = unsafe {
            sys::wlr_region_confine(region, from_x, from_y, to_x, to_y, &mut cx, &mut cy)
        };
        if ok { Some((cx, cy)) } else { None }
    }

    /// Warp the cursor to the layout point `(x, y)`, clamping to the nearest
    /// in-layout point if it falls outside the output layout
    /// ([`wlr_cursor_warp_closest`], which — unlike [`wlr_cursor_warp`] —
    /// always moves the cursor rather than no-op'ing on an out-of-bounds
    /// target). Passes a null device so no device mapping re-constrains the
    /// already-computed target. A no-op with no seat cursor.
    pub(crate) fn warp_cursor(&self, x: f64, y: f64) {
        let Some(cursor) = self.cursor_ptr() else {
            return;
        };
        // SAFETY: `cursor` is this runtime's own live cursor from `create_seat`;
        // a null device is explicitly allowed and means "ignore device mapping".
        unsafe { sys::wlr_cursor_warp_closest(cursor.as_ptr(), std::ptr::null_mut(), x, y) };
    }

    /// Send a relative-motion event to any `zwp_relative_pointer_v1` clients on
    /// `seat`, converting the event's millisecond timestamp to the microseconds
    /// the protocol carries. A no-op when no relative-pointer manager was
    /// created. Emitted on *every* pointer motion — constrained or not — since a
    /// relative-pointer client (a game, say) wants the raw deltas regardless of
    /// where the absolute cursor is, or whether it moved at all under a lock.
    ///
    /// # Safety
    ///
    /// `seat` must be a live `wlr_seat`.
    pub(crate) unsafe fn send_relative_pointer_motion(
        &self,
        seat: *mut sys::wlr_seat,
        time_msec: u32,
        dx: f64,
        dy: f64,
        unaccel_dx: f64,
        unaccel_dy: f64,
    ) {
        let Some(mgr) = self.relative_pointer_manager() else {
            return;
        };
        // SAFETY: `mgr` is the display-owned manager from
        // `create_relative_pointer_manager`, live as long as this runtime;
        // `seat` is live per the contract.
        unsafe {
            sys::wlr_relative_pointer_manager_v1_send_relative_motion(
                mgr.as_ptr(),
                seat,
                (time_msec as u64) * 1000,
                dx,
                dy,
                unaccel_dx,
                unaccel_dy,
            );
        }
    }

    /// Restore the confine invariant for `constraint`: if the cursor has ended
    /// up *outside* the constraint's region, warp it to a point genuinely
    /// inside, returning that point (so the caller can re-enter the surface
    /// under it). Returns `None` when no warp was needed or possible (cursor
    /// already inside, or the region is empty).
    ///
    /// This exists because [`wlr_region_confine`]'s `bool` keys off the *old*
    /// cursor position: a cursor left outside the region makes every future
    /// confine return `false`, wedging the pointer forever — and a client can
    /// trigger exactly that by moving its region off the cursor via
    /// `set_region` while the constraint is live. Re-anchoring the moment that
    /// can happen keeps the motion-path `false` arm effectively unreachable.
    ///
    /// The region is a set of rectangles that need not fill its bounding box,
    /// so clamping into the extents can land in a hole (inside the extents,
    /// outside the region) for a client-crafted L-shaped/split region. The
    /// extents-clamp is therefore only a *candidate*: if it is still outside,
    /// this falls back to clamping into the region's first rectangle, which is
    /// inside by construction. Rectangular regions — the common case — resolve
    /// on the first candidate, since a single rect's extents *is* the rect.
    ///
    /// # Safety
    ///
    /// `constraint` must point to a live `wlr_pointer_constraint_v1`.
    pub(crate) unsafe fn reanchor_cursor_into_region(
        &self,
        constraint: NonNull<sys::wlr_pointer_constraint_v1>,
    ) -> Option<(f64, f64)> {
        // SAFETY: `constraint` is live per the contract; `region` is an inline
        // field of it, and `data`/`extents` are plain fields of the
        // `pixman_region32_t` there.
        unsafe {
            let region = &raw const (*constraint.as_ptr()).region;
            let (cx, cy) = self.pointer_position();
            if self.region_contains_point(region, cx, cy) {
                // Invariant already holds — nothing to do.
                return None;
            }

            let ext = (*constraint.as_ptr()).region.extents;
            // Empty-region guard: pixman keeps an empty region's extents as a
            // degenerate box, so there is nowhere valid to put the cursor.
            // Never feed a garbage coordinate to the warp.
            if ext.x2 <= ext.x1 || ext.y2 <= ext.y1 {
                return None;
            }

            // Candidate 1: clamp into the extents box. Pixman boxes are
            // half-open `[x1, x2)`, so the inclusive upper bound is `x2 - 1`
            // (a non-empty box has `x2 > x1`, so `x2 - 1 >= x1` and the clamp
            // range is valid).
            let cand_x = cx.clamp(ext.x1 as f64, (ext.x2 - 1) as f64);
            let cand_y = cy.clamp(ext.y1 as f64, (ext.y2 - 1) as f64);
            let (wx, wy) = if self.region_contains_point(region, cand_x, cand_y) {
                (cand_x, cand_y)
            } else {
                // The extents-clamp landed in a hole; fall back to the first
                // rectangle, which is inside the region by construction.
                let rect = first_region_rect(region, ext);
                (
                    cx.clamp(rect.x1 as f64, (rect.x2 - 1) as f64),
                    cy.clamp(rect.y1 as f64, (rect.y2 - 1) as f64),
                )
            };

            self.warp_cursor(wx, wy);
            Some((wx, wy))
        }
    }

    /// Create the `zwp_relative_pointer_manager_v1` global, letting clients
    /// receive unaccelerated relative pointer motion events, independent of
    /// absolute cursor position. Errors if called twice.
    pub fn create_relative_pointer_manager(&self, display: &Display) -> Result<()> {
        if self.inner.relative_pointer_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_relative_pointer_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_relative_pointer_manager_v1_create(display.as_ptr()) };
        let raw =
            NonNull::new(raw).ok_or(Error::Create("wlr_relative_pointer_manager_v1_create"))?;
        *self.inner.relative_pointer_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The `zwp_relative_pointer_manager_v1` manager, once created via
    /// [`Runtime::create_relative_pointer_manager`] — used internally by
    /// [`send_relative_pointer_motion`](Runtime::send_relative_pointer_motion)
    /// to forward raw deltas to relative-pointer clients on every motion.
    pub(crate) fn relative_pointer_manager(
        &self,
    ) -> Option<NonNull<sys::wlr_relative_pointer_manager_v1>> {
        *self.inner.relative_pointer_manager.borrow()
    }

    /// Create the `ext_idle_notifier_v1` global. Clients (e.g. swayidle) bind
    /// `ext_idle_notification_v1` to be told when the seat has been idle for a
    /// timeout; this crate feeds it input activity from the seat handlers, and
    /// wlroots drives the client-facing timers. Errors if called twice.
    pub fn create_idle_notifier(&self, display: &Display) -> Result<()> {
        if self.inner.idle_notifier.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_idle_notifier called twice",
            ));
        }
        // SAFETY: display live for the call; notifier is display-owned and freed
        // with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_idle_notifier_v1_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_idle_notifier_v1_create"))?;
        *self.inner.idle_notifier.borrow_mut() = Some(raw);
        Ok(())
    }

    pub(crate) fn idle_notifier(&self) -> Option<NonNull<sys::wlr_idle_notifier_v1>> {
        *self.inner.idle_notifier.borrow()
    }

    /// Feed the idle notifier a user-activity event on the current seat, if
    /// both exist. Called from every seat-input dispatch in `backend.rs` (and
    /// the touch injectors in `runtime.rs`/test harness) so wlroots' idle
    /// timers reset on real activity. A no-op with no idle notifier or no
    /// seat — a consumer that never calls
    /// [`create_idle_notifier`](Runtime::create_idle_notifier) pays nothing
    /// for it.
    pub(crate) fn notify_seat_activity(&self) {
        let (Some(notifier), Some(seat)) = (self.idle_notifier(), self.seat_ptr()) else {
            return;
        };
        // SAFETY: `notifier` is display-owned and lives as long as this
        // runtime; `seat` was created by `create_seat` and lives as long as
        // this runtime.
        unsafe { sys::wlr_idle_notifier_v1_notify_activity(notifier.as_ptr(), seat.as_ptr()) };
    }

    /// Create the `zwp_idle_inhibit_manager_v1` global. A client (e.g. a
    /// video player) can bind it to inhibit idling for as long as one of its
    /// surfaces is visible; `backend.rs`'s `on_new_idle_inhibitor`/
    /// `on_idle_inhibitor_destroy` track how many inhibitors are currently
    /// live and re-gate [`create_idle_notifier`](Runtime::create_idle_notifier)'s
    /// notifier accordingly. Errors if called twice.
    pub fn create_idle_inhibit_manager(&self, display: &Display) -> Result<()> {
        if self.inner.idle_inhibit_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_idle_inhibit_manager called twice",
            ));
        }
        // SAFETY: display live for the call; the manager is display-owned and
        // freed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_idle_inhibit_v1_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_idle_inhibit_v1_create"))?;
        *self.inner.idle_inhibit_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    pub(crate) fn idle_inhibit_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_idle_inhibit_manager_v1>> {
        *self.inner.idle_inhibit_manager.borrow()
    }

    /// Gate the idle notifier on whether any idle inhibitor is currently
    /// live. Called from `backend.rs` every time `idle_inhibitors` changes —
    /// on a new inhibitor and on one being destroyed. A no-op with no idle
    /// notifier (a consumer that never calls
    /// [`create_idle_notifier`](Runtime::create_idle_notifier) pays nothing
    /// for it), regardless of whether an idle-inhibit manager exists.
    pub(crate) fn refresh_idle_inhibited(&self) {
        if let Some(notifier) = self.idle_notifier() {
            let inhibited = self.inner.idle_inhibitors.get() > 0;
            // SAFETY: `notifier` is display-owned and lives as long as this
            // runtime.
            unsafe { sys::wlr_idle_notifier_v1_set_inhibited(notifier.as_ptr(), inhibited) };
        }
    }

    /// Create the `ext_session_lock_manager_v1` global. A locker (a lock
    /// screen) binds it to lock the session; wlroots then drives the
    /// `ext-session-lock-v1` protocol and this crate's
    /// `backend.rs` handlers (`on_new_session_lock` and friends) enforce the
    /// state machine — the screen locks, input is refused to normal clients
    /// while locked, and a locker that dies without unlocking leaves the
    /// session locked. Errors if called twice.
    ///
    /// This only advertises the global. Whether it does anything depends on a
    /// [`Backend::run_all`](crate::Backend::run_all) being driven for the same
    /// `display`, which is what links the `new_lock` listener; a consumer that
    /// creates the manager and never runs takes no locks, exactly as for the
    /// other manager globals.
    pub fn create_session_lock_manager(&self, display: &Display) -> Result<()> {
        if self.inner.session_lock_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_session_lock_manager called twice",
            ));
        }
        // SAFETY: display live for the call; the manager is display-owned and
        // freed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_session_lock_manager_v1_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_session_lock_manager_v1_create"))?;
        *self.inner.session_lock_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    pub(crate) fn session_lock_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_session_lock_manager_v1>> {
        *self.inner.session_lock_manager.borrow()
    }

    /// Start Xwayland, so X11 clients can run on this compositor.
    ///
    /// Creates a `wlr_xwayland` over this runtime's own `wlr_compositor` (from
    /// [`init_graphics`](Runtime::init_graphics), which must have been called —
    /// this errors otherwise), advertising a `DISPLAY` that X11 clients connect
    /// to. With `lazy = true` the `Xwayland` process is spawned only when the
    /// first client connects, so a session with no X11 clients pays nothing.
    ///
    /// The manager is display/runtime-owned — no `Drop`, torn down with the
    /// display, matching the session-lock and idle managers — so this never
    /// frees it. Errors if called twice, or if graphics is not yet initialised,
    /// or if `wlr_xwayland_create` fails (the `Xwayland` binary is absent, most
    /// commonly — the caller is expected to treat that as non-fatal and run
    /// Wayland-only).
    ///
    /// Nothing happens on the wire until a [`Backend::run_all`](crate::Backend::run_all)
    /// drives the same `display`: that is what links the `ready`/`new_surface`
    /// listeners, exactly as for the other manager globals. On `ready` the crate
    /// points Xwayland at this runtime's seat itself (so the clipboard/DND
    /// bridge comes up), then calls
    /// [`ToplevelHandler::xwayland_ready`](crate::ToplevelHandler::xwayland_ready).
    #[cfg(wlr_has_xwayland)]
    pub fn create_xwayland(&self, display: &Display, lazy: bool) -> Result<()> {
        if self.inner.xwayland.borrow().is_some() {
            return Err(Error::Operation("Runtime::create_xwayland called twice"));
        }
        let compositor = self.inner.compositor.borrow().ok_or(Error::Operation(
            "Runtime::create_xwayland requires init_graphics first",
        ))?;
        // SAFETY: `display` is live for the call; `compositor` is this runtime's
        // own `wlr_compositor`, created by `init_graphics` over the same
        // display and never freed by this crate. The returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_xwayland_create(display.as_ptr(), compositor.as_ptr(), lazy) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_xwayland_create"))?;
        *self.inner.xwayland.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The `wlr_xwayland` manager, once created — read by `backend.rs` to link
    /// the `ready`/`new_surface` listeners.
    #[cfg(wlr_has_xwayland)]
    pub(crate) fn xwayland_ptr(&self) -> Option<NonNull<sys::wlr_xwayland>> {
        *self.inner.xwayland.borrow()
    }

    /// The `DISPLAY` name (`:N`) Xwayland advertises, or `None` before it is up
    /// (or if no Xwayland was created). This is the value a compositor exports
    /// as `DISPLAY` so X11 children connect here; valid only after
    /// [`ToplevelHandler::xwayland_ready`](crate::ToplevelHandler::xwayland_ready)
    /// has fired, since lazy start leaves it unset until then.
    #[cfg(wlr_has_xwayland)]
    pub fn xwayland_display_name(&self) -> Option<String> {
        let xwayland = self.xwayland_ptr()?;
        // SAFETY: `xwayland` is this runtime's own live manager; `display_name`
        // is a wlroots-owned C string (or null before the server is up), read
        // and copied out, never freed here.
        unsafe {
            let name = (*xwayland.as_ptr()).display_name;
            if name.is_null() {
                return None;
            }
            Some(
                std::ffi::CStr::from_ptr(name)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    /// Point Xwayland at this runtime's seat, so wlroots' `xwm` bridges the X11
    /// CLIPBOARD/PRIMARY selections and XDND to the Wayland seat. Called by the
    /// crate itself on `ready`; also public so a compositor can re-assert it
    /// after recreating the seat. A no-op if there is no Xwayland or no seat.
    #[cfg(wlr_has_xwayland)]
    pub fn set_xwayland_seat(&self) {
        let (Some(xwayland), Some(seat)) = (self.xwayland_ptr(), self.seat_ptr()) else {
            return;
        };
        // SAFETY: both are this runtime's own live objects, owned by the
        // display for as long as this runtime lives.
        unsafe { sys::wlr_xwayland_set_seat(xwayland.as_ptr(), seat.as_ptr()) };
    }

    /// Record a freshly-announced Xwayland surface. Called by
    /// `on_new_xwayland_surface`; the scene tree is filled in later, on
    /// `associate`.
    #[cfg(wlr_has_xwayland)]
    pub(crate) fn record_xwayland_surface(
        &self,
        id: XwaylandSurfaceId,
        raw: NonNull<sys::wlr_xwayland_surface>,
    ) {
        self.inner.xwayland_surfaces.borrow_mut().insert(
            id,
            XwaylandSurfaceEntry {
                raw,
                tree: std::cell::Cell::new(None),
            },
        );
    }

    /// Store (or clear, with `None`) an Xwayland surface's scene tree — set on
    /// `associate`, cleared on `unassociate`. A miss (unknown id) is a no-op.
    /// The tree is never destroyed by this crate; see [`XwaylandSurfaceEntry`].
    #[cfg(wlr_has_xwayland)]
    pub(crate) fn set_xwayland_surface_tree(
        &self,
        id: XwaylandSurfaceId,
        tree: Option<NonNull<sys::wlr_scene_tree>>,
    ) {
        if let Some(entry) = self.inner.xwayland_surfaces.borrow().get(&id) {
            entry.tree.set(tree);
        }
    }

    /// Forget `id`. Called from `on_xwayland_surface_destroy` before the
    /// surface is freed. Dropping the entry drops the stored (wlroots-owned)
    /// tree pointer without destroying it.
    #[cfg(wlr_has_xwayland)]
    pub(crate) fn forget_xwayland_surface(&self, id: XwaylandSurfaceId) {
        self.inner.xwayland_surfaces.borrow_mut().remove(&id);
    }

    /// The raw `wlr_xwayland_surface` `id` names, with the table borrow
    /// released before returning — the caller re-enters wlroots, which can
    /// take this same `RefCell`. `None` if this runtime knows no such surface.
    #[cfg(wlr_has_xwayland)]
    pub(crate) fn xwayland_surface_ptr(
        &self,
        id: XwaylandSurfaceId,
    ) -> Option<NonNull<sys::wlr_xwayland_surface>> {
        self.inner
            .xwayland_surfaces
            .borrow()
            .get(&id)
            .map(|e| e.raw)
    }

    /// Position and size the X11 window `id` names — the SSD content rect for a
    /// managed window, or the raw geometry for an override-redirect one. A miss
    /// (unknown id) is a no-op. Values are clamped into the `i16`/`u16` ranges
    /// the X11 wire carries.
    #[cfg(wlr_has_xwayland)]
    pub fn configure_xwayland_surface(&self, id: XwaylandSurfaceId, geometry: Box2D) {
        let Some(raw) = self.xwayland_surface_ptr(id) else {
            return;
        };
        let x = geometry.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let y = geometry.y.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let w = geometry.width.clamp(0, u16::MAX as i32) as u16;
        let h = geometry.height.clamp(0, u16::MAX as i32) as u16;
        // SAFETY: `raw` is a live surface — the entry is removed by
        // `on_xwayland_surface_destroy` before wlroots frees it, so a present
        // entry names a live one — and this only sends an X11 ConfigureNotify.
        unsafe { sys::wlr_xwayland_surface_configure(raw.as_ptr(), x, y, w, h) };
    }

    /// Send X11 focus-in/out to the window `id` names, so the client sees it as
    /// (de)activated. Paired with the seat's keyboard-enter path, which routes
    /// the actual keystrokes. A miss (unknown id) is a no-op.
    #[cfg(wlr_has_xwayland)]
    pub fn activate_xwayland_surface(&self, id: XwaylandSurfaceId, activated: bool) {
        let Some(raw) = self.xwayland_surface_ptr(id) else {
            return;
        };
        // SAFETY: `raw` is a live surface (see `configure_xwayland_surface`).
        unsafe { sys::wlr_xwayland_surface_activate(raw.as_ptr(), activated) };
    }

    /// Ask the X11 window `id` names to close (`WM_DELETE_WINDOW`, or a kill if
    /// it does not support the protocol) — backing the compositor's
    /// close-window action. A miss (unknown id) is a no-op.
    #[cfg(wlr_has_xwayland)]
    pub fn close_xwayland_surface(&self, id: XwaylandSurfaceId) {
        let Some(raw) = self.xwayland_surface_ptr(id) else {
            return;
        };
        // SAFETY: `raw` is a live surface (see `configure_xwayland_surface`).
        unsafe { sys::wlr_xwayland_surface_close(raw.as_ptr()) };
    }

    /// Reflect maximized state back to the X11 window `id` names, updating its
    /// `_NET_WM_STATE` so the client agrees with the compositor's model. wlroots
    /// carries horizontal and vertical maximization separately; the WM maximizes
    /// on both axes together, so this sets both. A miss (unknown id) is a no-op.
    #[cfg(wlr_has_xwayland)]
    pub fn set_xwayland_surface_maximized(&self, id: XwaylandSurfaceId, maximized: bool) {
        let Some(raw) = self.xwayland_surface_ptr(id) else {
            return;
        };
        // SAFETY: `raw` is a live surface (see `configure_xwayland_surface`);
        // this only updates `_NET_WM_STATE` on the X11 window.
        unsafe { sys::wlr_xwayland_surface_set_maximized(raw.as_ptr(), maximized, maximized) };
    }

    /// Reflect fullscreen state back to the X11 window `id` names, updating its
    /// `_NET_WM_STATE_FULLSCREEN`. A miss (unknown id) is a no-op.
    #[cfg(wlr_has_xwayland)]
    pub fn set_xwayland_surface_fullscreen(&self, id: XwaylandSurfaceId, fullscreen: bool) {
        let Some(raw) = self.xwayland_surface_ptr(id) else {
            return;
        };
        // SAFETY: `raw` is a live surface (see `configure_xwayland_surface`).
        unsafe { sys::wlr_xwayland_surface_set_fullscreen(raw.as_ptr(), fullscreen) };
    }

    /// Reflect minimized (iconified) state back to the X11 window `id` names,
    /// updating its `WM_STATE`/`_NET_WM_STATE_HIDDEN`. A miss (unknown id) is a
    /// no-op.
    #[cfg(wlr_has_xwayland)]
    pub fn set_xwayland_surface_minimized(&self, id: XwaylandSurfaceId, minimized: bool) {
        let Some(raw) = self.xwayland_surface_ptr(id) else {
            return;
        };
        // SAFETY: `raw` is a live surface (see `configure_xwayland_surface`).
        unsafe { sys::wlr_xwayland_surface_set_minimized(raw.as_ptr(), minimized) };
    }

    /// Restack the X11 window `id` names for Z-order parity with the WM's stack.
    ///
    /// With `sibling` set, the window is stacked directly above (`above`) or
    /// below the named sibling; with `sibling` `None` it moves to the top or the
    /// bottom of the whole stack. This is the X11 counterpart of raising a
    /// managed window in the scene graph. A miss (unknown id) is a no-op; an
    /// unknown `sibling` id is treated as `None` (stack to the top/bottom),
    /// never as a dangling pointer.
    #[cfg(wlr_has_xwayland)]
    pub fn restack_xwayland_surface(
        &self,
        id: XwaylandSurfaceId,
        sibling: Option<XwaylandSurfaceId>,
        above: bool,
    ) {
        let Some(raw) = self.xwayland_surface_ptr(id) else {
            return;
        };
        // A sibling we no longer know is resolved to null rather than a stale
        // pointer, so restack degrades to a plain top/bottom move.
        let sibling_ptr = sibling
            .and_then(|s| self.xwayland_surface_ptr(s))
            .map_or(std::ptr::null_mut(), |s| s.as_ptr());
        let mode = if above {
            sys::xcb_stack_mode_t::XCB_STACK_MODE_ABOVE
        } else {
            sys::xcb_stack_mode_t::XCB_STACK_MODE_BELOW
        };
        // SAFETY: `raw` is a live surface (see `configure_xwayland_surface`);
        // `sibling_ptr` is either null or another live surface from this same
        // table, and wlroots accepts null to mean "top/bottom of the stack".
        unsafe { sys::wlr_xwayland_surface_restack(raw.as_ptr(), sibling_ptr, mode) };
    }

    /// The scene tree the Xwayland surface `id` renders through, or `None` if
    /// this runtime knows no such surface or it has no associated surface yet
    /// (the tree is set on `associate`, cleared on `unassociate`). The `Cell`
    /// borrow is released before returning, so the caller may re-enter wlroots.
    #[cfg(wlr_has_xwayland)]
    pub(crate) fn xwayland_surface_tree_ptr(
        &self,
        id: XwaylandSurfaceId,
    ) -> Option<NonNull<sys::wlr_scene_tree>> {
        self.inner
            .xwayland_surfaces
            .borrow()
            .get(&id)
            .and_then(|e| e.tree.get())
    }

    /// Move the Xwayland surface `id`'s scene node. Coordinates are the scene's,
    /// which for a single output at the layout origin are the output's own —
    /// the same space [`set_toplevel_position`](Runtime::set_toplevel_position)
    /// works in. This is a compositor-side move of what is drawn; the X11 client
    /// is told its own geometry separately, via
    /// [`configure_xwayland_surface`](Runtime::configure_xwayland_surface). A
    /// miss (unknown id, or no associated surface yet) is a no-op.
    #[cfg(wlr_has_xwayland)]
    pub fn set_xwayland_surface_position(&self, id: XwaylandSurfaceId, x: i32, y: i32) {
        let Some(tree) = self.xwayland_surface_tree_ptr(id) else {
            return;
        };
        // SAFETY: the tree is the surface's scene node, created on `associate`
        // and cleared from the entry on `unassociate` before wlroots frees it,
        // so a present pointer names a live node.
        unsafe { sys::wlr_scene_node_set_position(&raw mut (*tree.as_ptr()).node, x, y) };
    }

    /// Show or hide the Xwayland surface `id`'s scene node — hiding, not
    /// unmapping, the same distinction
    /// [`set_toplevel_visible`](Runtime::set_toplevel_visible) draws: a window
    /// on an inactive workspace keeps its buffer and is simply not drawn. A
    /// miss is a no-op.
    #[cfg(wlr_has_xwayland)]
    pub fn set_xwayland_surface_visible(&self, id: XwaylandSurfaceId, visible: bool) {
        let Some(tree) = self.xwayland_surface_tree_ptr(id) else {
            return;
        };
        // SAFETY: as for `set_xwayland_surface_position`.
        unsafe { sys::wlr_scene_node_set_enabled(&raw mut (*tree.as_ptr()).node, visible) };
    }

    /// Raise the Xwayland surface `id`'s scene node above its siblings in the
    /// toplevel band — the X11 counterpart of
    /// [`raise_toplevel`](Runtime::raise_toplevel), and, like it, refused while
    /// a scene walk is live (wlroots iterates the band with the non-`_safe`
    /// `wl_list_for_each`, so unlinking and reinserting a node mid-walk would
    /// silently truncate the iteration). A miss is a no-op.
    #[cfg(wlr_has_xwayland)]
    pub fn raise_xwayland_surface(&self, id: XwaylandSurfaceId) {
        if self.scene_is_being_walked() {
            return;
        }
        let Some(tree) = self.xwayland_surface_tree_ptr(id) else {
            return;
        };
        // SAFETY: as for `set_xwayland_surface_position`.
        unsafe { sys::wlr_scene_node_raise_to_top(&raw mut (*tree.as_ptr()).node) };
    }

    /// Point the seat's keyboard at the Xwayland surface `id` names — the X11
    /// counterpart of [`focus_toplevel_keyboard`](Runtime::focus_toplevel_keyboard),
    /// and subject to the same lock gate: while the session is locked no normal
    /// surface may take keyboard focus, so this returns `None`. Also `None` for
    /// an unknown id, a surface with no `wlr_surface` yet, or one whose surface
    /// is not mapped. The paired X11 focus-in/out is sent separately by
    /// [`activate_xwayland_surface`](Runtime::activate_xwayland_surface).
    #[cfg(wlr_has_xwayland)]
    pub fn focus_xwayland_surface_keyboard(&self, id: XwaylandSurfaceId) -> Option<()> {
        // Input isolation while locked — see `focus_toplevel_keyboard`.
        if self.is_session_locked() {
            return None;
        }
        let seat = (*self.inner.seat.borrow())?;
        let xsurface = self.xwayland_surface_ptr(id)?;
        // SAFETY: a present entry names a live surface (its destroy listener
        // removes the entry before wlroots frees it). `surface` is null until
        // `associate`; the enter call tolerates a null keyboard by taking no
        // keycodes.
        unsafe {
            let surface = (*xsurface.as_ptr()).surface;
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

    /// The transient-parent (`WM_TRANSIENT_FOR`) of the Xwayland surface `id`
    /// names, as one of this runtime's own [`XwaylandSurfaceId`]s — the value a
    /// compositor centers a managed dialog over.
    ///
    /// `None` when `id` is unknown or stale, when the window set no parent, or
    /// when its parent is a surface this runtime no longer tracks (a stale
    /// pointer is never followed). The reverse lookup — raw pointer to id — is
    /// a scan of this runtime's small live-surface table.
    #[cfg(wlr_has_xwayland)]
    pub fn xwayland_surface_parent(&self, id: XwaylandSurfaceId) -> Option<XwaylandSurfaceId> {
        let raw = self.xwayland_surface_ptr(id)?;
        // SAFETY: a present entry names a live surface (its destroy listener
        // removes the entry before wlroots frees it); `parent` is a plain
        // pointer field, null when the window set no `WM_TRANSIENT_FOR`.
        let parent = unsafe { (*raw.as_ptr()).parent };
        if parent.is_null() {
            return None;
        }
        // Resolve the parent pointer back to an id we still track, so a caller
        // never receives an id for a freed surface.
        self.inner
            .xwayland_surfaces
            .borrow()
            .iter()
            .find(|(_, entry)| entry.raw.as_ptr() == parent)
            .map(|(pid, _)| *pid)
    }

    /// Reparent the Xwayland surface `id`'s scene tree into `band` — the seam a
    /// compositor drives to lift an override-redirect pop-up above
    /// [`Band::Toplevel`](crate::Band::Toplevel) (into
    /// [`Band::Top`](crate::Band::Top)), and to move a surface back down when it
    /// flips between the managed and unmanaged paths at runtime.
    ///
    /// The crate parents every Xwayland surface's tree into
    /// [`Band::Toplevel`](crate::Band::Toplevel) on `associate` (managed-window
    /// placement); an override-redirect surface belongs above it, and there is
    /// no per-surface way to know that at associate time (the flag can even flip
    /// after map), so band choice is the compositor's, applied through here.
    ///
    /// `None` when `id` is unknown or stale, when it has no associated surface
    /// yet (its tree is created on `associate`), or before
    /// [`init_graphics`](Runtime::init_graphics). Refused while a scene walk is
    /// live, for the same reason [`raise_xwayland_surface`](Runtime::raise_xwayland_surface)
    /// is — a reparent unlinks and reinserts the node, which corrupts wlroots'
    /// non-`_safe` band walk.
    #[cfg(wlr_has_xwayland)]
    pub fn reparent_xwayland_surface_to_band(
        &self,
        id: XwaylandSurfaceId,
        band: Band,
    ) -> Option<()> {
        if self.scene_is_being_walked() {
            return None;
        }
        let tree = self.xwayland_surface_tree_ptr(id)?;
        let band_tree = self.band_ptr(band)?;
        // SAFETY: `tree` is the surface's own scene node, created on `associate`
        // and cleared from the entry on `unassociate` before wlroots frees it,
        // so a present pointer names a live node; `band_tree` is one of the six
        // band trees created once in `init_graphics` and never destroyed while
        // this runtime lives.
        unsafe { sys::wlr_scene_node_reparent(&raw mut (*tree.as_ptr()).node, band_tree.as_ptr()) };
        Some(())
    }

    /// The Xwayland surface `id`'s scene-node position, relative to its parent
    /// band — the compositor-side placement last applied by
    /// [`set_xwayland_surface_position`](Runtime::set_xwayland_surface_position).
    /// `None` when `id` is unknown, stale, or has no associated surface yet.
    ///
    /// Read-only introspection, for tests that prove an override-redirect pop-up
    /// really landed at its client-requested coordinates.
    #[cfg(wlr_has_xwayland)]
    pub fn xwayland_surface_scene_position(&self, id: XwaylandSurfaceId) -> Option<(i32, i32)> {
        let tree = self.xwayland_surface_tree_ptr(id)?;
        // SAFETY: as for `set_xwayland_surface_position` — a present tree
        // pointer names a live node.
        Some(unsafe { ((*tree.as_ptr()).node.x, (*tree.as_ptr()).node.y) })
    }

    /// Which [`Band`](crate::Band) the Xwayland surface `id`'s scene tree is
    /// parented directly under, or `None` when `id` is unknown, stale, has no
    /// associated surface yet, or its tree is parented somewhere that is not one
    /// of the six bands.
    ///
    /// Read-only introspection, for tests that prove an override-redirect pop-up
    /// stacks in the band above managed toplevels.
    #[cfg(wlr_has_xwayland)]
    pub fn xwayland_surface_scene_parent_band(&self, id: XwaylandSurfaceId) -> Option<Band> {
        let tree = self.xwayland_surface_tree_ptr(id)?;
        // SAFETY: a present tree pointer names a live node; `parent` is a live
        // tree or null (never null here — a band-parented node always has one).
        let parent = unsafe { (*tree.as_ptr()).node.parent };
        if parent.is_null() {
            return None;
        }
        let g = self.inner.graphics.borrow();
        let g = g.as_ref()?;
        [
            Band::Background,
            Band::Bottom,
            Band::Toplevel,
            Band::Top,
            Band::Overlay,
            Band::Lock,
        ]
        .into_iter()
        .find(|&band| g.band_tree(band).as_ptr() == parent)
    }

    /// Whether the seat's keyboard is currently focused on the Xwayland surface
    /// `id` names. `None` when `id` is unknown, stale, or has no associated
    /// surface yet; `Some(false)` when the seat points elsewhere (or nowhere).
    ///
    /// Read-only introspection, for tests that prove a focus-taking
    /// override-redirect pop-up (a keyboard-navigable menu) actually holds the
    /// keyboard.
    #[cfg(wlr_has_xwayland)]
    pub fn xwayland_surface_has_keyboard_focus(&self, id: XwaylandSurfaceId) -> Option<bool> {
        let seat = (*self.inner.seat.borrow())?;
        let xsurface = self.xwayland_surface_ptr(id)?;
        // SAFETY: a present entry names a live surface; `surface` is null until
        // `associate`. Reading the seat's focused-surface pointer and comparing
        // it dereferences neither.
        unsafe {
            let surface = (*xsurface.as_ptr()).surface;
            if surface.is_null() {
                return None;
            }
            Some((*seat.as_ptr()).keyboard_state.focused_surface == surface)
        }
    }

    /// Drop every Xwayland surface this runtime knows of, without touching
    /// wlroots — the Xwayland counterpart of
    /// [`clear_toplevels`](Runtime::clear_toplevels), called by `run_inner`
    /// when the run that announced them returns. Ids are only meaningful for
    /// the run that announced them; the per-surface destroy listeners that
    /// would otherwise remove a stale entry are torn down with that run's
    /// `Session`.
    #[cfg(wlr_has_xwayland)]
    pub(crate) fn clear_xwayland_surfaces(&self) {
        self.inner.xwayland_surfaces.borrow_mut().clear();
    }

    /// Mint the next Xwayland surface id from the crate's process-wide counter.
    /// Called by `on_new_xwayland_surface`; separate so the counter stays the
    /// single source in [`crate::id`].
    #[cfg(wlr_has_xwayland)]
    pub(crate) fn next_xwayland_surface_id(&self) -> XwaylandSurfaceId {
        XwaylandSurfaceId(next_id())
    }

    /// Whether the session is currently locked.
    ///
    /// `true` from the instant a locker takes a lock until a **genuine**
    /// unlock — a locker that crashes or is killed without unlocking leaves
    /// this `true`, which is exactly what keeps the screen locked when the
    /// lock process dies. While this is `true`, this crate refuses keyboard
    /// and pointer focus to every normal toplevel and layer surface (see
    /// [`focus_toplevel_keyboard`](Runtime::focus_toplevel_keyboard),
    /// [`focus_layer_keyboard`](Runtime::focus_layer_keyboard) and
    /// [`toplevel_at`](Runtime::toplevel_at), and the crate-internal pointer
    /// hit test), so a consumer can observe the lock but never has to enforce
    /// it. Tests read this to prove the state machine.
    pub fn is_session_locked(&self) -> bool {
        self.inner.session_locked.get()
    }

    pub(crate) fn session_lock_ptr(&self) -> Option<NonNull<sys::wlr_session_lock_v1>> {
        *self.inner.session_lock.borrow()
    }

    /// Enter the locked state for a freshly-taken lock: mark the session
    /// locked, reset the per-lock flags, record the lock, and take keyboard
    /// focus away from whatever normal client had it. Called synchronously
    /// from `backend.rs`'s `on_new_session_lock` — the state change is applied
    /// here, immediately, not deferred behind the handler notification.
    pub(crate) fn begin_session_lock(&self, lock: NonNull<sys::wlr_session_lock_v1>) {
        self.inner.session_locked.set(true);
        self.inner.session_unlock_requested.set(false);
        self.inner.session_locked_sent.set(false);
        *self.inner.session_lock.borrow_mut() = Some(lock);
        // No normal client keeps keyboard focus across a lock.
        self.clear_keyboard_focus();
        // Cover every output with an opaque black fill beneath the lock
        // surfaces, so any uncovered region shows black, never normal content.
        self.install_lock_fill();
    }

    /// The extent of the whole output layout, `(x, y, width, height)`, or
    /// `None` when the layout is empty (no output placed) or graphics is not
    /// yet initialised. Unlike [`output_layout_box`](Runtime::output_layout_box),
    /// this passes a null reference output, which `wlr_output_layout_get_box`
    /// documents as returning the extents of the entire layout.
    fn output_layout_extent(&self) -> Option<(i32, i32, i32, i32)> {
        let layout = self.inner.graphics.borrow().as_ref().map(|g| g.layout)?;
        // SAFETY: `layout` is this runtime's own, created by `init_graphics`
        // and never freed by this crate (see [`Graphics`]'s own doc). A null
        // reference asks for the whole-layout extents;
        // `wlr_output_layout_get_box` fully initialises `dest_box` in every
        // case, so reading it back is sound.
        let wbox = unsafe {
            let mut wbox = std::mem::MaybeUninit::<sys::wlr_box>::uninit();
            sys::wlr_output_layout_get_box(
                layout.as_ptr(),
                std::ptr::null_mut(),
                wbox.as_mut_ptr(),
            );
            wbox.assume_init()
        };
        if wbox.width == 0 && wbox.height == 0 {
            None
        } else {
            Some((wbox.x, wbox.y, wbox.width, wbox.height))
        }
    }

    /// Create (or, on a crashed-locker takeover, reposition) the opaque black
    /// [`session_lock_fill`](RuntimeInner::session_lock_fill) so it covers the
    /// full current output-layout extent. Parented into [`Band::Lock`] before
    /// any lock surface arrives, so it sits at the bottom of the band and a
    /// live locker's surface still renders over it. A no-op if graphics is not
    /// initialised or the layout is empty (no output to cover yet) — the
    /// crate rates a hotplug-during-lock resize LOW, so the fill covers the
    /// outputs present at lock time and is not re-sized on later hotplug.
    fn install_lock_fill(&self) {
        let Some((x, y, w, h)) = self.output_layout_extent() else {
            return;
        };
        // A takeover after a locker crash calls `begin_session_lock` again
        // while the previous fill is still live: reposition it rather than
        // leaking a second rect over the first.
        if let Some(existing) = self.inner.session_lock_fill.get() {
            self.set_rect_size(existing, w, h);
            self.set_rect_position(existing, x, y);
            return;
        }
        // Opaque black, premultiplied. `add_rect_in_band` appends at the end
        // of the lock band's children (topmost within the band); because no
        // lock surface has been added yet, later surfaces append above it.
        if let Ok(id) = self.add_rect_in_band(Band::Lock, w, h, [0.0, 0.0, 0.0, 1.0]) {
            self.set_rect_position(id, x, y);
            self.inner.session_lock_fill.set(Some(id));
        }
    }

    /// Destroy the opaque black lock fill, if present. Called only on a
    /// genuine unlock — a locker dying keeps the session locked, so its fill
    /// must remain to cover the now-uncovered outputs.
    fn remove_lock_fill(&self) {
        // Take the id only once the destroy has actually happened.
        //
        // `remove_rect` refuses while a node borrow is live, and the fill is
        // the opaque black rect that hides the desktop under a lock. Taking
        // first meant that on the refused path the rect survived with its id
        // gone: the session unlocks, the screen stays black, and there is no
        // longer an id to remove it by. Unrecoverable from safe code, from a
        // path with no error to report.
        let Some(id) = self.inner.session_lock_fill.get() else {
            return;
        };
        if self.remove_rect(id).is_some() {
            self.inner.session_lock_fill.set(None);
            return;
        }
        // `remove_rect` missed, and the two reasons need opposite answers.
        //
        // A live node borrow refused it: the rect is still there, so keep the
        // id — a later unlock retries and the fill is still removable.
        //
        // The row is already gone (a cascade destroyed it, or the node API
        // did): there is nothing left to destroy, and holding the id is
        // actively harmful. `install_lock_fill` early-returns on a `Some`
        // fill and repositions it, so a latched dead id makes every
        // *subsequent* lock install no fill at all — the session locks with
        // nothing covering the outputs the locker has not painted. Unconditional
        // `take()` self-healed this for free; recovering the refusal case cost
        // it, so it is restored explicitly here.
        if !self.inner.rects.borrow().contains_key(&id) {
            self.inner.session_lock_fill.set(None);
        }
    }

    /// Record a lock surface's scene tree, keyed by the output it covers, so
    /// the coverage check can find it and the destroy path can drop it. Called
    /// from `on_session_lock_new_surface` after the tree is created in
    /// [`Band::Lock`].
    pub(crate) fn record_lock_surface(
        &self,
        output: *mut sys::wlr_output,
        tree: NonNull<sys::wlr_scene_tree>,
        surface: *mut sys::wlr_surface,
    ) {
        self.inner
            .lock_surface_trees
            .borrow_mut()
            .insert(output as usize, LockSurfaceRender { tree, surface });
    }

    /// Drop the lock surface covering `output` from the tree map. Called from
    /// `on_session_lock_surface_destroy` before wlroots frees the tree. Only
    /// removes this crate's reference — wlroots owns and frees the subsurface
    /// tree itself when the surface dies, so this must never call
    /// `wlr_scene_node_destroy` on it.
    pub(crate) fn forget_lock_surface(&self, output: *mut sys::wlr_output) {
        self.inner
            .lock_surface_trees
            .borrow_mut()
            .remove(&(output as usize));
    }

    /// Whether every live output is covered by a lock surface whose underlying
    /// `wlr_surface` is mapped (has committed a buffer). This is the
    /// precondition for sending the protocol's `locked` event. `false` when
    /// there are no live outputs — sending `locked` with nothing on screen
    /// would be a security hole, so an empty output set never satisfies it.
    pub(crate) fn all_outputs_lock_covered(&self) -> bool {
        let outputs = self.inner.outputs.borrow();
        if outputs.is_empty() {
            return false;
        }
        let trees = self.inner.lock_surface_trees.borrow();
        for out in outputs.values() {
            match trees.get(&(out.as_ptr() as usize)) {
                Some(entry) => {
                    if entry.surface.is_null() {
                        return false;
                    }
                    // SAFETY: the surface is live while its lock surface is —
                    // the per-surface destroy listener removes this entry
                    // before wlroots frees the surface — so reading `mapped`
                    // is sound. `mapped` is wlroots' own flag, true once the
                    // client has committed a buffer.
                    if !unsafe { (*entry.surface).mapped } {
                        return false;
                    }
                }
                None => return false,
            }
        }
        true
    }

    /// Send the protocol's `locked` event exactly once, if every output is now
    /// covered and it has not already been sent. Called from
    /// `on_session_lock_surface_commit` on every lock surface commit; the
    /// `session_locked_sent` guard makes the repeated calls idempotent.
    pub(crate) fn send_locked_if_covered(&self) {
        if self.inner.session_locked_sent.get() {
            return;
        }
        if !self.all_outputs_lock_covered() {
            return;
        }
        let Some(lock) = self.session_lock_ptr() else {
            return;
        };
        // SAFETY: `lock` is the active lock recorded by `begin_session_lock`
        // and cleared by `on_session_lock_destroy` before wlroots frees it, so
        // it is live here. `wlr_session_lock_v1_send_locked` is exactly the
        // call the protocol requires once every output is covered.
        unsafe { sys::wlr_session_lock_v1_send_locked(lock.as_ptr()) };
        self.inner.session_locked_sent.set(true);
    }

    /// Handle a **genuine** unlock request from the active locker: record that
    /// an unlock was asked for (so a following lock `destroy` completes the
    /// teardown instead of staying locked), drop the lock surfaces, and leave
    /// the session unlocked. Called synchronously from
    /// `on_session_unlock`; the `session_lock_changed(false)` handler
    /// notification is emitted separately by the caller.
    pub(crate) fn unlock_session(&self) {
        self.inner.session_unlock_requested.set(true);
        self.inner.lock_surface_trees.borrow_mut().clear();
        self.inner.session_locked.set(false);
        self.inner.session_locked_sent.set(false);
        // Genuine unlock: the outputs are about to show normal content again,
        // so the opaque cover must go. A locker *dying* never reaches here
        // (`take_lock_destroy_was_unlocked` keeps the session locked), so the
        // fill correctly survives a crash.
        self.remove_lock_fill();
    }

    /// Common cleanup when the active `wlr_session_lock_v1` is destroyed: clear
    /// the lock pointer, drop any remaining surface trees, and reset the
    /// send-once flag. Returns whether an unlock had been requested first —
    /// `true` means a genuine unlock already ran and the caller completes the
    /// teardown; `false` means the locker died without unlocking and the
    /// caller must **keep the session locked** (the security invariant). This
    /// method never touches [`session_locked`](RuntimeInner::session_locked),
    /// so the stay-locked path is preserved by construction.
    pub(crate) fn take_lock_destroy_was_unlocked(&self) -> bool {
        *self.inner.session_lock.borrow_mut() = None;
        self.inner.lock_surface_trees.borrow_mut().clear();
        self.inner.session_locked_sent.set(false);
        self.inner.session_unlock_requested.replace(false)
    }

    /// Give the keyboard focus to a lock surface's underlying `wlr_surface`.
    ///
    /// Unlike [`focus_toplevel_keyboard`](Runtime::focus_toplevel_keyboard)
    /// and [`focus_layer_keyboard`](Runtime::focus_layer_keyboard), this is
    /// **not** gated on the lock state — it is the one surface class allowed
    /// focus while locked, and is called from `on_session_lock_new_surface`
    /// for the first lock surface so the lock screen can receive keystrokes.
    /// A no-op with no seat or a null surface.
    ///
    /// # Safety
    ///
    /// `surface` must be null or a live `wlr_surface`.
    pub(crate) unsafe fn focus_lock_surface_keyboard(&self, surface: *mut sys::wlr_surface) {
        let seat = *self.inner.seat.borrow();
        let Some(seat) = seat else { return };
        if surface.is_null() {
            return;
        }
        // SAFETY: `surface` is live per this method's contract;
        // `wlr_seat_get_keyboard` returns null when no keyboard is attached,
        // which the enter call tolerates by taking no keycodes — the identical
        // shape `focus_toplevel_keyboard` follows.
        unsafe {
            if (*seat.as_ptr()).keyboard_state.focused_surface == surface {
                return;
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

    /// The renderer [`init_graphics`](Runtime::init_graphics) created, as a
    /// **non-owning** view.
    ///
    /// `None` before `init_graphics` has run.
    ///
    /// Borrowed rather than owned, and that is the whole point of the separate
    /// type: this renderer is deliberately never destroyed (see `Graphics`' own
    /// doc for why, and what a future `Drop` would have to do), so handing out
    /// a [`Renderer`](crate::Renderer) — which destroys what it holds — would
    /// make a double free one `drop` away. A renderer this crate's consumer
    /// owns comes from [`Renderer::autocreate`](crate::Renderer::autocreate)
    /// instead.
    ///
    /// The `&self` borrow is what bounds the view: it cannot outlive the
    /// `Runtime` handle it was taken from, and every clone of that handle names
    /// the same graphics.
    pub fn renderer_ref(&self) -> Option<RendererRef<'_>> {
        let raw = self.inner.graphics.borrow().as_ref().map(|g| g.renderer)?;
        // SAFETY: `init_graphics` created this renderer and nothing destroys it
        // — `Graphics` has no `Drop`, and wlroots never destroys a renderer it
        // did not create — so it is live for the whole process, which outlives
        // this borrow. The view cannot free it.
        Some(unsafe { RendererRef::from_raw(raw.as_ptr()) })
    }

    /// The allocator [`init_graphics`](Runtime::init_graphics) created, as a
    /// **non-owning** view. `None` before `init_graphics` has run.
    ///
    /// Borrowed for the same reason [`renderer_ref`](Runtime::renderer_ref) is.
    pub fn allocator_ref(&self) -> Option<AllocatorRef<'_>> {
        let raw = self.inner.graphics.borrow().as_ref().map(|g| g.allocator)?;
        // SAFETY: as in `renderer_ref`; the allocator is created once by
        // `init_graphics` and never destroyed.
        Some(unsafe { AllocatorRef::from_raw(raw.as_ptr()) })
    }

    /// The renderer's `events.lost` signal, for `backend.rs`'s per-run
    /// listener. `None` before [`init_graphics`](Runtime::init_graphics) has
    /// run — in which case there is no renderer to watch and no listener is
    /// registered, exactly as for the optional globals.
    pub(crate) fn renderer_ptr(&self) -> Option<NonNull<sys::wlr_renderer>> {
        self.inner.graphics.borrow().as_ref().map(|g| g.renderer)
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

    /// The scene tree `band` names — any of the six bands, including
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

    /// The maximum popup nesting this crate will walk.
    ///
    /// wlroots cannot produce a cycle — a popup's parent is fixed when the role
    /// object is created and a client cannot re-parent one — and no real menu
    /// is anywhere near this deep. The cap is not about correctness of the
    /// happy path; it is about the failure mode. Every walk here runs on the
    /// compositor's input path, and an unbounded walk over a corrupted table
    /// would hang the session, which a user cannot distinguish from a freeze. A
    /// bounded walk returns a wrong-but-finite answer instead, and the tests
    /// that forge a cycle pin that it does.
    pub(crate) const MAX_POPUP_DEPTH: usize = 64;

    /// Record a newly-announced popup under `id`.
    ///
    /// Called from `backend.rs`'s `on_new_popup`, before the handler is told,
    /// mirroring [`record_toplevel`](Runtime::record_toplevel).
    // `backend.rs`'s `on_new_popup` is wired up by a later task in this
    // part; only this module's own tests call this until then.
    #[allow(dead_code)]
    pub(crate) fn record_popup(
        &self,
        id: PopupId,
        raw: NonNull<sys::wlr_xdg_popup>,
        tree: NonNull<sys::wlr_scene_tree>,
        parent: PopupParent,
    ) {
        self.inner.popups.borrow_mut().insert(
            id,
            PopupEntry {
                raw,
                tree,
                parent,
                configured: std::cell::Cell::new(false),
            },
        );
    }

    /// Remove `id`'s entry. Called from `on_popup_destroy` before the popup is
    /// freed, mirroring [`forget_toplevel`](Runtime::forget_toplevel).
    ///
    /// **Does not destroy the scene tree**, and must never grow that: a popup's
    /// tree is a child of its parent's, wlroots frees a tree's children
    /// recursively, and this runs while the parent may already be dying. See
    /// [`PopupEntry`]'s own doc.
    ///
    /// Children of this popup are **not** removed here. wlroots destroys a
    /// popup's own children first and emits a `destroy` for each, so each child
    /// removes itself through this same path; sweeping them here would race
    /// that and drop rows a still-pending emission is about to use.
    // `on_popup_destroy` is wired up by a later task in this part; only
    // this module's own tests call this until then.
    #[allow(dead_code)]
    pub(crate) fn forget_popup(&self, id: PopupId) {
        self.inner.popups.borrow_mut().remove(&id);
    }

    /// This id's recorded raw popup, with the borrow released before returning
    /// — see [`toplevel_entry`](Runtime::toplevel_entry)'s own doc for why that
    /// is not optional.
    pub(crate) fn popup_raw(&self, id: PopupId) -> Option<NonNull<sys::wlr_xdg_popup>> {
        self.inner.popups.borrow().get(&id).map(|e| e.raw)
    }

    /// This id's recorded scene subtree, as [`popup_raw`](Runtime::popup_raw).
    #[allow(dead_code)] // wired up by a later task in this part
    pub(crate) fn popup_tree(&self, id: PopupId) -> Option<NonNull<sys::wlr_scene_tree>> {
        self.inner.popups.borrow().get(&id).map(|e| e.tree)
    }

    /// Mark this popup as having been configured at least once.
    #[allow(dead_code)] // wired up by a later task in this part
    pub(crate) fn mark_popup_configured(&self, id: PopupId) {
        if let Some(entry) = self.inner.popups.borrow().get(&id) {
            entry.configured.set(true);
        }
    }

    /// Drop every popup this runtime knows of, without touching wlroots.
    ///
    /// Called once by `backend.rs`'s `run_inner` when the `run_all` call that
    /// populated the table returns, on every exit path — mirroring
    /// [`clear_toplevels`](Runtime::clear_toplevels) exactly, and for the
    /// identical reason: a popup id is only meaningful for the call that
    /// announced it, because the per-popup destroy listener that would remove a
    /// stale row is itself torn down with that call's `Session`. Without this, a
    /// consumer who kept a `Runtime` clone could resolve a stale id and hand
    /// wlroots memory it had already freed.
    // `run_inner`'s call site is wired up by a later task in this part;
    // only this module's own tests call this until then.
    #[allow(dead_code)]
    pub(crate) fn clear_popups(&self) {
        self.inner.popups.borrow_mut().clear();
    }

    /// Borrow the popup `id` names, for as long as the borrow lasts.
    ///
    /// `None` once the popup is gone — the by-id miss every id type in this
    /// crate promises.
    pub fn popup(&self, id: PopupId) -> Option<Popup<'_>> {
        let (raw, parent) = {
            let popups = self.inner.popups.borrow();
            let entry = popups.get(&id)?;
            (entry.raw, entry.parent)
        };
        // SAFETY: an entry is removed by `on_popup_destroy`, which wlroots runs
        // before it frees the popup, so a present entry names a live one. The
        // borrow above is released before the handle is built, because the
        // caller will re-enter wlroots, which can emit a signal, which can take
        // the same `RefCell` mutably.
        Some(unsafe { Popup::from_raw_with_id(raw.as_ptr(), id, parent) })
    }

    /// What this popup hangs off, or `None` if it is gone.
    pub fn popup_parent(&self, id: PopupId) -> Option<PopupParent> {
        self.inner.popups.borrow().get(&id).map(|e| e.parent)
    }

    /// The **direct** children of `parent`, in creation order.
    ///
    /// Creation order is the z-order tiebreak among siblings, so this is the
    /// order a compositor's own popup stack should record them in.
    pub fn popups_of(&self, parent: PopupParent) -> Vec<PopupId> {
        let popups = self.inner.popups.borrow();
        let mut out: Vec<PopupId> = popups
            .iter()
            .filter(|(_, entry)| entry.parent == parent)
            .map(|(id, _)| *id)
            .collect();
        // The table is a `HashMap`, so iteration order is arbitrary; ids come
        // from a monotonic counter, so sorting by the id *is* sorting by
        // creation order. This is the one place in the crate that orders ids,
        // and it does so through the raw `u64` rather than an `Ord` impl on
        // `PopupId` precisely so the public type keeps promising nothing about
        // ordering (see `ToplevelId`'s own doc).
        out.sort_unstable_by_key(|id| id.0);
        out
    }

    /// Every popup in the subtree under `parent`, parents before their own
    /// children, deepest last.
    ///
    /// Breadth-first over [`popups_of`](Runtime::popups_of), capped at
    /// `Runtime::MAX_POPUP_DEPTH` levels (private, so not linked here) and
    /// de-duplicated,
    /// so a corrupted table yields a finite list with no id twice rather than a
    /// hang or a double destroy. Reverse this for the order xdg-shell requires
    /// popups to be destroyed in.
    pub fn popup_chain(&self, parent: PopupParent) -> Vec<PopupId> {
        let mut out: Vec<PopupId> = Vec::new();
        let mut frontier = vec![parent];
        for _ in 0..Self::MAX_POPUP_DEPTH {
            if frontier.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for p in frontier.drain(..) {
                for child in self.popups_of(p) {
                    if out.contains(&child) {
                        continue;
                    }
                    out.push(child);
                    next.push(PopupParent::Popup(child));
                }
            }
            frontier = next;
        }
        out
    }

    /// Place `id` inside `constraint` and answer its configure.
    ///
    /// Two wlroots calls in sequence — `wlr_xdg_popup_unconstrain_from_box`,
    /// which rewrites `scheduled.geometry` from the client's rules, then
    /// `wlr_xdg_surface_schedule_configure`, which is the only way to actually
    /// send it. There is no `wlr_xdg_popup_set_size`/`_configure`; this pair is
    /// the whole placement API.
    ///
    /// **`constraint` is in the root toplevel/layer parent surface's coordinate
    /// system**, not layout or output space. wlroots' own header says so, and
    /// getting it wrong places every popup at an offset rather than failing
    /// loudly. The compositor is what translates its output's usable area into
    /// that space.
    ///
    /// `false` when the popup is gone, or when its surface is not yet
    /// `initialized`.
    ///
    /// Unlike [`Popup::send_configure`](crate::Popup::send_configure), which
    /// guards its own call, `wlr_xdg_popup_unconstrain_from_box` in this
    /// distribution's wlroots reaches into `wlr_xdg_surface_schedule_configure`
    /// **itself**, unconditionally — there is no equivalent guard inside it. On
    /// an uninitialized surface (`base->surface` naming no live client yet)
    /// that dereferences dead state and crashes the process; it is not the
    /// "costs nothing, touches no wire" no-op an earlier draft of this method
    /// assumed. So this checks `initialized` itself, first, and skips
    /// `unconstrain` entirely rather than merely skipping `send_configure`, as
    /// the original two-call design here intended. A real popup announced over
    /// the wire always has this become true well before a caller could reach
    /// this method from a live event, so nothing a compositor does with an
    /// already-mapped popup changes; it is this crate's own scratch fixtures —
    /// and any other caller reaching for `configure_popup` a tick too early —
    /// that this now turns away instead of aborting.
    pub fn configure_popup(&self, id: PopupId, constraint: &Box2D) -> bool {
        // SAFETY: `raw` names a live `wlr_xdg_popup` (see `popup_raw`'s own
        // doc); `base` is only compared against null and, if non-null, only
        // has its plain `bool` field read — no call into wlroots yet.
        let initialized = match self.popup_raw(id) {
            Some(raw) => unsafe {
                let base = (*raw.as_ptr()).base;
                !base.is_null() && (*base).initialized
            },
            None => return false,
        };
        if !initialized {
            return false;
        }
        let Some(popup) = self.popup(id) else {
            return false;
        };
        popup.unconstrain(constraint);
        if popup.send_configure() == 0 {
            return false;
        }
        self.mark_popup_configured(id);
        true
    }

    /// This popup's position in its **parent surface's** coordinates, or `None`
    /// if it is gone.
    pub fn popup_position(&self, id: PopupId) -> Option<(f64, f64)> {
        Some(self.popup(id)?.position())
    }

    /// Destroy `id` and every popup under it, **deepest first**, and report how
    /// many were destroyed.
    ///
    /// Deepest-first is not a preference: `xdg_popup.destroy` on a popup that
    /// still has live children is a protocol error, and wlroots enforces it.
    ///
    /// Each destroy sends `xdg_popup.popup_done` and makes the resource inert;
    /// wlroots emits `events.destroy` from inside the call, which runs
    /// `on_popup_destroy`, which is what actually removes the row. Nothing here
    /// touches a scene tree.
    pub fn dismiss_popup(&self, id: PopupId) -> usize {
        let mut order = self.popup_chain(PopupParent::Popup(id));
        order.push(id);
        let mut destroyed = 0;
        // Reversed: `popup_chain` is shallow-first, and destroying a parent
        // before its children is the protocol error above.
        for victim in order.into_iter().rev() {
            let Some(popup) = self.popup(victim) else {
                // Already gone — wlroots destroys a popup's children with it,
                // so a deeper row may have been swept by an earlier iteration's
                // own destroy emission. A miss here is expected, not an error.
                continue;
            };
            popup.destroy();
            destroyed += 1;
        }
        destroyed
    }

    /// Destroy every popup hanging off `parent`, chains and all, deepest first.
    /// Returns how many were destroyed.
    ///
    /// This is what a compositor calls when the window or layer surface a menu
    /// belongs to goes away, and what P2's "a click outside dismisses the whole
    /// chain" path falls back to for **non-grabbing** popups (a grabbing chain
    /// is wlroots' own to dismiss — see [`Popup::grab_requested`]).
    pub fn dismiss_popups_of(&self, parent: PopupParent) -> usize {
        let mut destroyed = 0;
        for child in self.popups_of(parent) {
            destroyed += self.dismiss_popup(child);
        }
        destroyed
    }

    /// `(*popup).seat != NULL` — whether this popup's client sent
    /// `xdg_popup.grab`.
    ///
    /// `false` for an unknown id. See [`Popup::grab_requested`], and this
    /// crate's `popup` module doc, for what a `true` means the compositor must
    /// **not** do.
    pub fn popup_is_grabbing(&self, id: PopupId) -> bool {
        self.popup(id).is_some_and(|p| p.grab_requested())
    }

    /// Whether *some* explicit seat grab is in force right now —
    /// `wlr_seat_pointer_has_grab(seat) || wlr_seat_keyboard_has_grab(seat)`.
    ///
    /// An xdg-popup grab is one; a drag-and-drop grab is another. This is the
    /// single fact a compositor's focus synchronisation needs: while it is
    /// `true`, wlroots is routing pointer, keyboard and touch itself, will
    /// dismiss the popup chain on a press outside it, and will restore the
    /// pre-grab keyboard focus when the grab ends — so a compositor that also
    /// moves focus is fighting it. P2's `sync_seat_focus` returns early on this.
    ///
    /// `false` with no seat: this is called from focus paths that run before
    /// [`create_seat`](Runtime::create_seat) has, and answering "no grab" there
    /// is both true and safe.
    pub fn seat_has_explicit_grab(&self) -> bool {
        let seat = *self.inner.seat.borrow();
        let Some(seat) = seat else {
            return false;
        };
        // SAFETY: `seat` is this runtime's own `wlr_seat`, created by
        // `create_seat` and live for as long as the runtime; both predicates
        // only read `seat->{pointer,keyboard}_state.grab`.
        unsafe {
            sys::wlr_seat_pointer_has_grab(seat.as_ptr())
                || sys::wlr_seat_keyboard_has_grab(seat.as_ptr())
        }
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
        // Refused while a scene borrow or buffer walk is live. wlroots
        // iterates with `wl_list_for_each`, not the `_safe` variant, so
        // unlinking a node and reinserting it elsewhere mid-walk leaves the
        // iteration reading `link.next` from where the node used to be — it
        // silently stops early rather than crashing, which is worse. The
        // destroy calls refuse for this reason; the restacks unlink just as
        // thoroughly and did not, until this was added alongside them.
        if self.scene_is_being_walked() {
            return None;
        }
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
        scene: NonNull<sys::wlr_scene_layer_surface_v1>,
        band: Layer,
    ) {
        self.inner.layer_surfaces.borrow_mut().insert(
            id,
            LayerSurfaceEntry {
                raw,
                scene_tree,
                scene,
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
        // runtime's own scene still owns; `band` is one of the six band
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
        // Input isolation: while the session is locked, no normal layer
        // surface may take keyboard focus either — see
        // [`focus_toplevel_keyboard`](Runtime::focus_toplevel_keyboard).
        if self.is_session_locked() {
            return None;
        }
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
        // Input isolation: while the session is locked, no normal toplevel may
        // take keyboard focus, whatever a consumer asks for. This is one of
        // the two focus gates that make the lock safe — see
        // [`is_session_locked`](Runtime::is_session_locked).
        if self.is_session_locked() {
            return None;
        }
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
        // Input isolation: while the session is locked, no normal toplevel is
        // ever "under the pointer" as far as click-to-focus/raise/move is
        // concerned — only lock surfaces exist to the pointer, and they carry
        // no [`ToplevelId`]. Returning `None` unconditionally here keeps a
        // consumer's own pointer routing from ever resolving a toplevel while
        // locked, the pointer half of the same guarantee the keyboard gates
        // give. Pointer *forwarding* to the lock surface itself still works —
        // that goes through [`leaf_surface_at`](Runtime::leaf_surface_at),
        // which is restricted to the lock band rather than disabled.
        if self.is_session_locked() {
            return None;
        }
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

    /// The drag icon's current layout position, if a drag with a visible icon
    /// is in progress. `None` when no drag is active, or the active drag
    /// carries no icon. Intended for tests asserting the icon renders and
    /// tracks the input; the position is the scene node's layout coordinates
    /// (`wlr_scene_node_coords`).
    ///
    /// Contrary to what an earlier revision of this doc claimed,
    /// `wlr_scene_drag_icon` does **not** self-track the pointer/touch
    /// position — verified against wlroots 0.20.2's own C source, its only
    /// reposition listener fires on the icon surface's own buffer-commit
    /// deltas, never on cursor motion. Left alone, the icon renders once at
    /// `(0, 0)` and never moves. So this crate repositions the node itself,
    /// on every pointer/touch motion, for as long as a drag icon is active —
    /// see `backend.rs`'s `on_pointer_motion`, `on_pointer_motion_absolute`,
    /// and [`inject_touch_motion`](Runtime::inject_touch_motion), all three
    /// of which call `reposition_drag_icon` (crate-internal)
    /// with the cursor's/touch point's own layout coordinates after updating
    /// them. A `Some` returned here always reflects that live position, not
    /// a stale snapshot from when the drag started. The icon's own hotspot
    /// offset is applied separately, by wlroots' internal `surface_tree`
    /// commit handler on the node this tree parents — this crate positions
    /// the *outer* tree at the raw cursor, matching tinywl's own
    /// `wlr_scene_node_set_position(&drag_icon->node, cursor->x, cursor->y)`.
    pub fn drag_icon_position(&self) -> Option<(i32, i32)> {
        let tree = (*self.inner.drag_icon_tree.borrow())?;
        let mut lx = 0;
        let mut ly = 0;
        // SAFETY: a `Some` stored here is always a live `wlr_scene_tree` —
        // `backend.rs`'s `on_start_drag` populates this cell with the tree
        // `wlr_scene_drag_icon_create` just returned, and the destroy
        // listener it registers on the icon's `events.destroy` clears the
        // cell back to `None` before wlroots frees the tree. Reads only
        // ever happen on the thread driving this runtime's event loop, the
        // same thread every listener above runs on, so there is no window in
        // which this could observe a tree mid-teardown.
        let found = unsafe {
            sys::wlr_scene_node_coords(&raw mut (*tree.as_ptr()).node, &raw mut lx, &raw mut ly)
        };
        found.then_some((lx, ly))
    }

    /// Move the drag-icon scene node to `(x, y)` — layout coordinates, the
    /// same space [`drag_icon_position`](Runtime::drag_icon_position) reads
    /// back — if a drag icon is currently active. A no-op with no active
    /// drag icon.
    ///
    /// Called from `backend.rs`'s `on_pointer_motion`/
    /// `on_pointer_motion_absolute` and from
    /// [`inject_touch_motion`](Runtime::inject_touch_motion), each time
    /// *after* they have already updated the cursor's/touch point's own
    /// position for the motion, with that same freshly-updated position —
    /// this is the fix for `wlr_scene_drag_icon` not self-tracking input;
    /// see [`drag_icon_position`](Runtime::drag_icon_position)'s own doc for
    /// the full story. `pub(crate)`, not part of this crate's public
    /// surface: a consumer never calls this directly, it is wired into the
    /// crate's own motion handlers.
    pub(crate) fn reposition_drag_icon(&self, x: i32, y: i32) {
        // The `NonNull` is copied out and the borrow dropped *before* the
        // FFI call, rather than held across it: `wlr_scene_node_set_position`
        // does not re-enter this crate today, but nothing about this method
        // depends on that staying true, and every other call site in this
        // crate that touches a `RefCell`-guarded pointer across an FFI call
        // follows the same "copy out, then call" discipline to avoid a
        // future re-entrant borrow panicking.
        let Some(tree) = *self.inner.drag_icon_tree.borrow() else {
            return;
        };
        // SAFETY: `tree` is a live `wlr_scene_tree` for the same reason
        // `drag_icon_position`'s SAFETY comment gives — a `Some` here is
        // always live, guaranteed by the destroy listener `on_start_drag`
        // registers on the icon's own `events.destroy`.
        unsafe { sys::wlr_scene_node_set_position(&raw mut (*tree.as_ptr()).node, x, y) };
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
        // Input isolation: while the session is locked, restrict the hit test
        // to the lock band, so the only surfaces the pointer can ever resolve
        // to are the locker's own — a normal toplevel or layer surface under
        // the cursor is never returned, and so never receives pointer
        // enter/motion (this method is the sole pointer-focus path, via
        // `backend.rs`'s `enter_surface_under_cursor`). The lock band's own
        // node is the hit-test root then; unlocked, it is the scene root as
        // before. If there is no lock band (graphics never initialised) while
        // somehow locked, nothing is hittable and the pointer resolves to
        // nothing — fail closed.
        let root = if self.is_session_locked() {
            let lock_band = self.band_ptr(Band::Lock)?;
            // SAFETY: `lock_band` is this runtime's own scene tree from
            // `init_graphics`, live for the call.
            unsafe { &raw mut (*lock_band.as_ptr()).node }
        } else {
            // SAFETY: the scene is this runtime's own and outlives the call.
            unsafe { &raw mut (*scene.as_ptr()).tree.node }
        };
        // SAFETY: `root` is a live scene node (either the scene root or the
        // lock band, both this runtime's own); the two out-parameters are live
        // stack locals.
        let node = unsafe { sys::wlr_scene_node_at(root, x, y, &raw mut nx, &raw mut ny) };
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
        self.notify_seat_activity();
        let seat = self.seat_ptr()?;
        let (surface, sx, sy) = self.leaf_surface_at(x, y)?;
        // SAFETY: `seat` is this runtime's own live seat (from
        // `seat_ptr`); `surface` was just resolved from a hit test against
        // this runtime's own live scene and is therefore live too.
        // wlroots reads `sx`/`sy` by value and does not retain them.
        Some(unsafe {
            sys::wlr_seat_touch_notify_down(seat.as_ptr(), surface, time_msec, id, sx, sy)
        })
    }

    /// Test-only: synthesize a touch-motion to `(x, y)` for the touch point
    /// `id`, continuing a drag started with
    /// [`inject_touch_down`](Runtime::inject_touch_down). Re-resolves the
    /// surface-local coordinates at the new position via
    /// [`leaf_surface_at`](Runtime::leaf_surface_at) and forwards to
    /// `wlr_seat_touch_notify_motion`. It also calls
    /// `wlr_seat_touch_point_focus` first, so the touch point's focus
    /// surface tracks whatever is under it as it moves — required for a
    /// drag, whose destination surface differs from the touch-down one
    /// (see the SAFETY note for detail).
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
        self.notify_seat_activity();
        let Some(seat) = self.seat_ptr() else {
            return;
        };
        let Some((surface, sx, sy)) = self.leaf_surface_at(x, y) else {
            return;
        };
        // SAFETY: `seat` is this runtime's own live seat; `surface` was
        // just resolved from a hit test against this runtime's own live
        // scene and is therefore live too; `sx`/`sy` are read by value and
        // not retained.
        //
        // `wlr_seat_touch_point_focus` first: unlike a pointer, a touch
        // point's `focus_surface` is not recomputed on every motion --
        // `wlr_seat_touch_notify_motion` alone keeps delivering to whatever
        // surface last held focus (the down surface, absent a call here),
        // which is fine for plain touch input but wrong for a drag, where
        // the destination surface under the point changes as it moves.
        // Calling this every motion (not only when the surface changes) is
        // deliberate and cheap -- wlroots' own touch_point_focus is a no-op
        // when `surface` already matches the point's current focus, so this
        // just keeps `focus_surface`/`sx`/`sy` in sync with the hit test
        // unconditionally rather than this crate tracking "did it change"
        // itself and risking drift from wlroots' own idea of the same
        // question.
        unsafe {
            sys::wlr_seat_touch_point_focus(seat.as_ptr(), surface, time_msec, id, sx, sy);
            sys::wlr_seat_touch_notify_motion(seat.as_ptr(), time_msec, id, sx, sy);
        }

        // `(x, y)` are already layout coordinates — this method's own doc
        // says so — so no conversion is needed before handing them to
        // `reposition_drag_icon`, unlike the pointer motion handlers, which
        // read layout coordinates back out of `wlr_cursor` first. `.round()`,
        // not truncation: matches the pointer handlers, and for the same
        // reason — truncating toward zero on an f64 that lands just under a
        // whole pixel leaves the icon a pixel short of where the point
        // actually is.
        self.reposition_drag_icon(x.round() as i32, y.round() as i32);
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
        self.notify_seat_activity();
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

    /// Push `shape` at the `wlr_cursor`, where `None` means the crate's
    /// default `left_ptr` image and `Some(s)` the xcursor
    /// `wlr_cursor_shape_v1_name(s)` names.
    ///
    /// Loads the theme on the first call — lazily, from a pointer event
    /// rather than eagerly in `create_seat`, so a consumer that never gets a
    /// pointer device never touches the filesystem for a theme it does not
    /// need — and then short-circuits when the same value is already
    /// applied.
    ///
    /// The theme load is deliberately *above* the short-circuit: a load that
    /// failed must be retried on the next pointer event, and with nothing
    /// named `applied_cursor` reaches its steady state after one call, so a
    /// short-circuit placed first would make the retry unreachable and leave
    /// a themeless system with no cursor image forever.
    ///
    /// A no-op with no seat: there is no `wlr_cursor`/`wlr_xcursor_manager`
    /// pair to set an image on before [`Runtime::create_seat`] has run.
    fn apply_cursor(&self, shape: Option<CursorShape>) {
        let (Some(cursor), Some(xcursor)) = (self.cursor_ptr(), *self.inner.xcursor.borrow())
        else {
            return;
        };
        if !self.inner.cursor_image_loaded.get() {
            // The `bool` is the load's own success. Latching `true`
            // regardless would turn a themeless system (no cursor theme
            // installed, or `XCURSOR_THEME` naming one that is not there)
            // into a permanent no-image cursor, because no later call would
            // ever retry the load.
            //
            // No log on failure: this crate binds no Rust-side logging
            // symbol (wlroots' own `wlr_log` is a `static inline` macro over
            // an unbound `_wlr_log`, and the crate deliberately has no
            // `log`/`tracing` dependency), so the retry *is* the report — a
            // theme that appears later is picked up.
            //
            // SAFETY: `xcursor` was created by `create_seat` and lives as
            // long as this runtime; `wlr_xcursor_manager_load` is safe to
            // call more than once (idempotent per its own header doc).
            if unsafe { sys::wlr_xcursor_manager_load(xcursor.as_ptr(), 1.0) } {
                self.inner.cursor_image_loaded.set(true);
            }
        }
        if self.inner.applied_cursor.get() == Some(shape) {
            return;
        }
        // SAFETY: both pointers were created together by `create_seat` and
        // live as long as this runtime. `wlr_cursor_shape_v1_name` returns a
        // pointer into a static, null-terminated table for every shape
        // `CursorShape::to_raw` can produce — it never returns null for a
        // value this crate's own enum encodes. `wlr_cursor_set_xcursor` is
        // safe to call unconditionally.
        unsafe {
            let name = match shape {
                Some(shape) => sys::wlr_cursor_shape_v1_name(shape.to_raw()),
                None => c"left_ptr".as_ptr(),
            };
            sys::wlr_cursor_set_xcursor(cursor.as_ptr(), xcursor.as_ptr(), name);
        }
        self.inner.applied_cursor.set(Some(shape));
    }

    /// Make sure the cursor has an image, loading the default xcursor theme
    /// on the first call and setting the `left_ptr` image whenever the
    /// cursor has none. Called from every pointer motion/button callback in
    /// `backend.rs` rather than once at `create_seat` time, so a consumer
    /// that never gets a pointer device pays nothing for a theme it never
    /// needed.
    ///
    /// "Whenever the cursor has none" is load-bearing: a shape named through
    /// [`Runtime::set_cursor_shape`] survives every pointer motion until the
    /// pointer focus changes (`backend.rs`'s `on_pointer_focus_change`) or
    /// the consumer names [`CursorShape::Default`]. Before 0.20.26 this
    /// forced `left_ptr` unconditionally, so a named shape lived exactly
    /// until the client's next motion event.
    ///
    /// A no-op with no seat.
    pub(crate) fn ensure_cursor_image(&self) {
        self.apply_cursor(self.inner.named_cursor.get());
    }

    /// Set the cursor image to the xcursor `wlr_cursor_shape_v1_name(shape)`
    /// names, loaded from the theme [`Runtime::create_seat`] already set up.
    /// The intended caller is a compositor's own
    /// [`crate::SeatHandler::request_set_shape`] override, applying the shape
    /// a `cursor-shape-v1` client asked for.
    ///
    /// The shape *persists*: it survives every subsequent pointer motion and
    /// button event, and is reset — to the default `left_ptr` — only when
    /// wlroots' pointer focus moves to a different surface (or to none at
    /// all, which is what a client disconnecting produces), or when a caller
    /// names [`CursorShape::Default`] here. A consumer therefore never has
    /// to hit-test its own model geometry to work out when to "un-set" a
    /// client's cursor. Read the current state back with
    /// [`Runtime::cursor_shape`].
    ///
    /// [`CursorShape::Default`] is the one shape that does not become the
    /// named cursor: it *clears* the named cursor, restoring exactly the
    /// pre-0.20.26 behaviour for a consumer that never names anything else.
    /// `CursorShape::Pointer` is a distinct shape — `cursor-shape-v1`
    /// defines it as "pointer that indicates a link or another interactive
    /// element", i.e. the hand cursor — not an alias for
    /// [`CursorShape::Default`], so it persists like any other named shape.
    ///
    /// Naming the shape that is already the named cursor is a no-op.
    ///
    /// The focus-change reset described above is installed only if
    /// [`Runtime::create_seat`] ran *before* the backend registered this
    /// run's listeners; a seat created later gets shape persistence but no
    /// reset, and its consumer is back to deciding for itself when a named
    /// shape stops applying. This is the same pre-existing ordering
    /// constraint the seat's other signals live under —
    /// `request_set_cursor`, `request_set_selection` and `start_drag` are
    /// likewise only wired for a seat that already exists at registration
    /// time — so a compositor that creates its seat in `main` before running
    /// the backend, which is the documented shape of a consumer, is
    /// unaffected.
    ///
    /// A no-op with no seat — mirrors `Runtime::ensure_cursor_image`'s own
    /// "no seat, nothing to set" rule, for the same reason: there is no
    /// `wlr_cursor`/`wlr_xcursor_manager` pair to set an image on before
    /// [`Runtime::create_seat`] has run.
    pub fn set_cursor_shape(&self, shape: CursorShape) {
        if self.cursor_ptr().is_none() {
            return;
        }
        let named = match shape {
            CursorShape::Default => None,
            shape => Some(shape),
        };
        if self.inner.named_cursor.get() == named && self.inner.applied_cursor.get().is_some() {
            return;
        }
        self.inner.named_cursor.set(named);
        self.apply_cursor(named);
    }

    /// The shape a `cursor-shape-v1` client named through
    /// [`Runtime::set_cursor_shape`] and that is still in force, or `None`
    /// when the cursor is showing the default `left_ptr` image.
    ///
    /// This is the crate's own record of the state described on
    /// [`Runtime::set_cursor_shape`]: it goes back to `None` on a
    /// pointer-focus change and on [`CursorShape::Default`], so a consumer
    /// can render or assert on "is a client naming the cursor right now?"
    /// without tracking focus itself.
    pub fn cursor_shape(&self) -> Option<CursorShape> {
        self.inner.named_cursor.get()
    }

    /// Drop any named cursor and go back to the default `left_ptr` image.
    ///
    /// Called from `backend.rs`'s `on_pointer_focus_change` — wlroots emits
    /// `wlr_seat.pointer_state.events.focus_change` both when the pointer
    /// enters a different surface and when it leaves for none at all
    /// (including when the naming client dies, since wlroots clears pointer
    /// focus as part of tearing a client's surfaces down), so this one hook
    /// covers every way the shape a client named stops applying.
    pub(crate) fn reset_named_cursor(&self) {
        if self.inner.named_cursor.get().is_none() {
            return;
        }
        self.inner.named_cursor.set(None);
        self.apply_cursor(None);
    }

    /// Test-only: what was last handed to `wlr_cursor_set_xcursor`, in
    /// `named_cursor`'s encoding (`Some(None)` = the default `left_ptr`,
    /// outer `None` = nothing applied yet).
    ///
    /// `#[cfg(test)]` rather than exported: the image actually on the cursor
    /// is wlroots' private state (`wlr_cursor.state` is `WLR_PRIVATE`), so
    /// this is the only way for a test to distinguish "`ensure_cursor_image`
    /// left the named shape alone" from "it stomped it back to `left_ptr`",
    /// which is the whole point of the 0.20.26 change. Consumers get
    /// [`Runtime::cursor_shape`], which is about intent rather than about
    /// which FFI calls were made.
    #[cfg(test)]
    pub(crate) fn applied_cursor(&self) -> Option<Option<CursorShape>> {
        self.inner.applied_cursor.get()
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

    /// Whether [`enable_test_touch`](Runtime::enable_test_touch) has been
    /// called. Read by `backend.rs`'s `update_seat_capabilities` on every
    /// recompute, exactly as `has_keyboard`/`has_pointer` are.
    pub(crate) fn test_touch_enabled(&self) -> bool {
        self.inner.test_touch_enabled.get()
    }

    /// Test-only: make the seat advertise `WL_SEAT_CAPABILITY_TOUCH` so
    /// headless clients can bind `wl_touch` and injected touch points
    /// ([`inject_touch_down`](Runtime::inject_touch_down) and friends) are
    /// accepted. There is no touch input device; this exists purely so
    /// `inject_touch_*` work in a test harness. No production caller.
    ///
    /// wlroots' own `wlr_seat_touch.c` (`touch_point_create`) refuses to
    /// create a touch point unless the target surface's client already
    /// holds a `wl_touch` resource, and a client can only obtain one via
    /// `wl_seat.get_touch`, which wlroots gates server-side on the seat
    /// having advertised the touch capability. Nothing in this crate ever
    /// sets that bit otherwise — `on_new_input` has no
    /// `WLR_INPUT_DEVICE_TOUCH` arm, since there is no virtual-touch
    /// Wayland protocol to receive one from — so without this method a
    /// headless seat driven only by `inject_touch_*` can never produce a
    /// touch point at all.
    ///
    /// Sets the flag `update_seat_capabilities` now reads on every future
    /// recompute (see [`RuntimeInner::test_touch_enabled`]'s own doc for why
    /// that matters — a one-shot capability set here would be clobbered by
    /// the very next keyboard or pointer hot-plug), then immediately
    /// triggers a recompute itself, so a seat that already exists starts
    /// advertising touch right away rather than waiting for some unrelated
    /// device event to happen to fire one.
    #[doc(hidden)]
    pub fn enable_test_touch(&self) {
        self.inner.test_touch_enabled.set(true);
        crate::backend::update_seat_capabilities(self);
    }
}

/// The first rectangle of a pixman region, guaranteed inside the region by
/// construction — the re-anchor fallback for a non-rectangular region whose
/// extents-clamp landed in a hole.
///
/// A pixman region is either a single rectangle equal to its `extents` (when
/// `data` is null) or a `data` header immediately followed by its rectangle
/// array (pixman's decades-stable `PIXREGION_BOXPTR` layout — the boxes start
/// one `pixman_region32_data` past `data`). The exact field layout this reads
/// (`pixman_region32` / `pixman_region32_data` / `pixman_box32`) is pinned by
/// `wlr-sys`'s own `const _` layout assertions, so this is not a guess about an
/// opaque type. `extents` is passed in (already read by the caller) as the
/// null-`data` answer and the `numRects == 0` backstop.
///
/// # Safety
///
/// `region` must point to a live `pixman_region32_t`; `extents` must be its
/// (non-empty) extents box, so the fallback is only ever taken for a
/// genuinely non-empty region.
unsafe fn first_region_rect(
    region: *const sys::pixman_region32_t,
    extents: sys::pixman_box32,
) -> sys::pixman_box32 {
    // SAFETY: `region` is live per the contract. `data` null ⇒ the region is a
    // single rect equal to `extents`. Otherwise the boxes follow the header:
    // `(data as *const pixman_region32_data).add(1)` is exactly pixman's
    // `PIXREGION_BOXPTR`, and a non-empty region has `numRects >= 1`, so the
    // first box is initialised. The `numRects == 0` arm cannot arise for the
    // non-empty region the caller guarantees, but is handled for total safety.
    unsafe {
        let data = (*region).data;
        if data.is_null() || (*data).numRects == 0 {
            extents
        } else {
            let boxes =
                (data as *const sys::pixman_region32_data).add(1) as *const sys::pixman_box32;
            *boxes
        }
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

    /// The frozen fix for M-2: the six bands exist, are direct children of
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
            band_link(g.lock_band),
        ];

        assert_eq!(
            actual, expected,
            "the six bands must be the scene root's first six children, \
             in Background < Bottom < toplevels < Top < Overlay < Lock order"
        );

        let overlay_pos = actual
            .iter()
            .position(|&p| p == band_link(g.overlay_band))
            .expect("overlay band is a root child");
        let lock_pos = actual
            .iter()
            .position(|&p| p == band_link(g.lock_band))
            .expect("lock band is a root child");
        assert!(
            overlay_pos < lock_pos,
            "the lock band must sit above the overlay band: session-lock \
             surfaces must cover even Overlay layer-shell content while the \
             session is locked"
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
            NonNull::<sys::wlr_scene_layer_surface_v1>::dangling(),
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
            NonNull::<sys::wlr_scene_layer_surface_v1>::dangling(),
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
            let p =
                unsafe { alloc_zeroed(Layout::new::<sys::wlr_output>()) }.cast::<sys::wlr_output>();
            NonNull::new(p).expect("allocation failed")
        };
        let out_b = {
            let p =
                unsafe { alloc_zeroed(Layout::new::<sys::wlr_output>()) }.cast::<sys::wlr_output>();
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
            NonNull::<sys::wlr_scene_layer_surface_v1>::dangling(),
            Layer::Background,
        );
        rt.record_layer_surface(
            ls_b,
            NonNull::from(&mut ls_on_b),
            NonNull::<sys::wlr_scene_tree>::dangling(),
            NonNull::<sys::wlr_scene_layer_surface_v1>::dangling(),
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

    /// No drag has ever started, so `drag_icon_tree` is still the `None`
    /// [`Runtime::new`] initialised it to — this is as much of
    /// [`Runtime::drag_icon_position`] as a unit test can exercise cheaply.
    /// Driving a real drag end-to-end (a visible icon rendered as a scene
    /// node, then following injected pointer motion) needs a client and a
    /// running seat, which is exactly what the downstream icedtea-wm harness
    /// render/follow test — not this crate's unit tests — covers.
    #[test]
    fn drag_icon_position_is_none_with_no_active_drag() {
        let rt = headless_runtime();
        assert_eq!(rt.drag_icon_position(), None);
    }

    /// The fix this whole chain of doc corrections exists to justify:
    /// `reposition_drag_icon` actually moves the node, and
    /// `drag_icon_position` reads the move back. Does not need a real drag —
    /// `reposition_drag_icon` only ever touches whatever tree
    /// `drag_icon_tree` names, so a real scene tree planted directly (the
    /// same "stands in for the tree a callback would have handed a
    /// production code path" trick
    /// `reparent_layer_surface_if_changed_moves_the_tree_only_when_the_layer_changed`
    /// uses) is enough to exercise it without a client or a running seat.
    #[test]
    fn reposition_drag_icon_moves_the_tree_and_drag_icon_position_reads_it_back() {
        let rt = headless_runtime();

        // Stands in for the tree `on_start_drag` would have stored, parented
        // under the same Overlay band that function uses.
        // SAFETY: `overlay` is a live tree owned by `rt`'s own scene.
        let overlay = rt.band_ptr(Band::Overlay).expect("overlay band");
        let tree = NonNull::new(unsafe { sys::wlr_scene_tree_create(overlay.as_ptr()) }).unwrap();
        *rt.inner.drag_icon_tree.borrow_mut() = Some(tree);

        assert_eq!(rt.drag_icon_position(), Some((0, 0)));

        rt.reposition_drag_icon(42, 17);
        assert_eq!(rt.drag_icon_position(), Some((42, 17)));

        // A second move overwrites rather than accumulates — layout
        // coordinates, not a delta.
        rt.reposition_drag_icon(3, 900);
        assert_eq!(rt.drag_icon_position(), Some((3, 900)));
    }

    /// With no active drag icon, `reposition_drag_icon` is a documented
    /// no-op — this is what makes it safe for `on_pointer_motion` and
    /// `inject_touch_motion` to call unconditionally on every motion, active
    /// drag or not, rather than checking first.
    #[test]
    fn reposition_drag_icon_is_a_no_op_with_no_active_drag() {
        let rt = headless_runtime();
        rt.reposition_drag_icon(5, 5);
        assert_eq!(rt.drag_icon_position(), None);
    }

    /// The claim the whole node-id scheme rests on, checked against wlroots
    /// rather than assumed: destroying a tree runs the addon destructor of
    /// **every** node beneath it, not only of the node named.
    ///
    /// If that were ever untrue, a descendant's row would survive its node and
    /// the next `set_node_position` on it would write through a freed pointer.
    ///
    /// The witness is per-thread (see `scene::NODE_DESTROY_COUNT`), so this
    /// delta measures this cascade and not whatever another test running beside
    /// it happened to destroy.
    #[test]
    fn destroying_a_tree_runs_every_descendants_addon_destructor() {
        let rt = headless_runtime();
        let band = rt.band_node(Band::Overlay).expect("overlay band");
        let top = rt.create_tree_in_band(Band::Overlay).expect("top tree");
        let middle = rt.create_tree_under(top).expect("middle tree");
        let leaf = rt
            .create_rect(middle, 4, 4, [1.0, 0.0, 0.0, 1.0])
            .expect("leaf");

        let before = crate::scene::node_destroy_count();
        assert_eq!(rt.destroy_node(top), Some(()));
        assert_eq!(
            crate::scene::node_destroy_count() - before,
            3,
            "the cascade must free the payload of the tree, its child tree \
             and the rect — not only the node destroy_node was handed"
        );

        for stale in [top, middle, leaf] {
            assert_eq!(rt.node_kind(stale), None, "every descendant id is stale");
            assert_eq!(rt.set_node_position(stale, 1, 1), None);
            assert_eq!(rt.destroy_node(stale), None, "and misses, not double-frees");
        }
        assert!(
            rt.node_children(band).expect("band is a tree").is_empty(),
            "the band is empty again"
        );
    }

    /// The bridge in the other direction: a rect from the frozen 0.20.1 API
    /// carries a node id, and destroying it through the node API purges the
    /// `RectId` row too — which is what stops `remove_rect` afterwards from
    /// calling `wlr_scene_node_destroy` on memory wlroots already reclaimed.
    #[test]
    fn destroying_a_legacy_rect_by_node_id_purges_its_rect_row() {
        let rt = headless_runtime();
        let rect = rt
            .add_rect_in_band(Band::Top, 8, 8, [0.0, 0.0, 1.0, 1.0])
            .expect("rect");
        let node = rt.rect_node(rect).expect("a legacy rect has a node id");

        assert_eq!(rt.destroy_node(node), Some(()));

        assert!(
            !rt.inner.rects.borrow().contains_key(&rect),
            "the addon destructor must drop the RectId row as well"
        );
        assert_eq!(rt.remove_rect(rect), None, "and remove_rect now misses");
        assert_eq!(rt.node_kind(node), None);
    }

    /// Nothing under a held lock is mutable through the `NodeId` API, not
    /// just the band itself.
    ///
    /// Refusing on the band alone missed the only node that matters: the
    /// opaque black fill *inside* it, created through `add_rect_in_band` like
    /// any other rect and so `Owned` and fully mutable. Two public calls reach
    /// it — `band_node(Band::Lock)` then `node_children` — after which
    /// hiding, destroying or walking it off-screen uncovers a live desktop
    /// under a session the compositor still reports as locked.
    #[test]
    fn nothing_under_a_held_lock_is_mutable_through_the_node_api() {
        let rt = headless_runtime();
        // Built exactly as `install_lock_fill` builds it — an opaque black
        // rect appended to the lock band — without needing an output layout,
        // which a unit-test runtime has none of. The origin is what matters
        // here, and `add_rect_in_band` is the same call it makes.
        let fill_rect = rt
            .add_rect_in_band(Band::Lock, 8, 8, [0.0, 0.0, 0.0, 1.0])
            .expect("fill");
        rt.inner.session_lock_fill.set(Some(fill_rect));
        let band = rt.band_node(Band::Lock).expect("lock band");
        let fill = *rt
            .node_children(band)
            .expect("band children")
            .first()
            .expect("the fill is a child of the lock band");

        // Unlocked, the fill is an ordinary owned rect.
        assert_eq!(rt.set_node_enabled(fill, false), Some(()));
        assert_eq!(rt.set_node_enabled(fill, true), Some(()));

        rt.inner.session_locked.set(true);
        assert_eq!(
            rt.set_node_enabled(fill, false),
            None,
            "hiding the fill uncovers the desktop just as hiding the band does"
        );
        assert_eq!(
            rt.set_node_position(fill, 99_999, 99_999),
            None,
            "walking it off-screen is the same uncovering by another route"
        );
        assert_eq!(
            rt.destroy_node(fill),
            None,
            "and destroying it outright certainly is"
        );
        // The band itself is still covered by the same rule.
        assert_eq!(rt.set_node_enabled(band, false), None);

        // A node outside the lock band is untouched by any of this.
        let elsewhere = rt
            .add_rect_in_band(Band::Top, 4, 4, [1.0, 0.0, 0.0, 1.0])
            .expect("rect");
        let elsewhere = rt.rect_node(elsewhere).expect("node id");
        assert_eq!(rt.set_node_enabled(elsewhere, false), Some(()));
        assert_eq!(rt.set_node_position(elsewhere, 5, 5), Some(()));

        rt.inner.session_locked.set(false);
        // And once unlocked the fill is ordinary again — the refusal is scoped
        // to the lock, not a permanent quarantine.
        assert_eq!(rt.set_node_enabled(fill, false), Some(()));
    }

    /// A lock fill whose row died elsewhere must not latch its id forever.
    ///
    /// `remove_lock_fill` used an unconditional `take()`, which self-healed
    /// for free. Keeping the id across a borrow-refused destroy cost that, and
    /// a latched dead id is worse than the bug it fixed: `install_lock_fill`
    /// early-returns on a `Some` fill and repositions it, so every *later*
    /// lock installs no fill at all and the session locks with nothing
    /// covering the outputs the locker has not painted.
    #[test]
    fn a_lock_fill_whose_row_died_elsewhere_is_reinstalled_not_latched() {
        let rt = headless_runtime();
        let first = rt
            .add_rect_in_band(Band::Lock, 8, 8, [0.0, 0.0, 0.0, 1.0])
            .expect("fill");
        rt.inner.session_lock_fill.set(Some(first));

        // Kill the row the way a cascade would, behind the Cell's back.
        assert_eq!(rt.remove_rect(first), Some(()));
        assert!(
            rt.inner.session_lock_fill.get().is_some(),
            "the Cell still names the dead rect — this is the state to recover from"
        );

        rt.remove_lock_fill();
        assert_eq!(
            rt.inner.session_lock_fill.get(),
            None,
            "a fill that no longer exists must stop being named, or \
             install_lock_fill's early return makes every later lock bare"
        );

        // The refusal case still keeps its id, which is what the conditional
        // clear was introduced for and must not regress.
        let live = rt
            .add_rect_in_band(Band::Lock, 8, 8, [0.0, 0.0, 0.0, 1.0])
            .expect("fill");
        rt.inner.session_lock_fill.set(Some(live));
        let node = rt.rect_node(live).expect("node id");
        rt.with_node(node, |_| {
            rt.remove_lock_fill();
        })
        .expect("borrowable");
        assert_eq!(
            rt.inner.session_lock_fill.get(),
            Some(live),
            "a destroy refused by a live borrow must keep the id to retry with"
        );
    }

    /// A foreign frame refuses scene restructuring on its own, with no
    /// `NodeBorrowGuard` beside it.
    ///
    /// Seven of the eight sites that enter a `ForeignFrame` pair it with a
    /// `NodeBorrowGuard`, and that pairing was what actually refused destroys
    /// — the frame itself refused only the event loop. `render::sync`'s
    /// timeline waiter is the eighth and has no runtime to raise a guard on,
    /// so a callback there could commit a scene output and, because no
    /// dispatcher is in dispatch, have `output_sample` delivered synchronously
    /// into a handler that destroys a node mid-commit. wlroots asserts on
    /// that, which on Arch is a dead process.
    ///
    /// Tested directly rather than through the timeline: that path needs a DRM
    /// node, so it self-skips on a GPU-less runner and would prove nothing
    /// there. The property is the frame, not the caller.
    #[test]
    fn a_foreign_frame_alone_refuses_scene_restructuring() {
        let rt = headless_runtime();
        let band = rt.band_node(Band::Overlay).expect("band");
        let doomed = rt.create_tree_under(band).expect("tree");

        assert!(
            !rt.scene_is_being_walked(),
            "nothing is walking the scene yet"
        );

        {
            // Exactly what sync.rs's waiter enters, and nothing else.
            let _frame = crate::dispatch::ForeignFrame::enter();
            assert!(rt.scene_is_being_walked());
            assert_eq!(
                rt.destroy_node(doomed),
                None,
                "wlroots may be mid-wl_list_for_each; unlinking is a UAF in its recursion"
            );
            assert_eq!(
                rt.create_tree_under(band).map(|_| ()),
                None,
                "and inserting rewires the tail its cursor is about to read"
            );
            // Restacking relinks, so it is refused for the same reason.
            let sibling = rt.band_node(Band::Top).expect("band");
            assert_eq!(rt.reparent_node(doomed, sibling), None);

            // Position is *not* refused, and should not be:
            // `wlr_scene_node_set_position` writes x/y into the node and
            // touches no list, so it cannot disturb a cursor. The rule is
            // about relinking, not about mutation in general.
            assert_eq!(rt.set_node_position(doomed, 1, 1), Some(()));
        }

        // The frame is a scope, not a latch.
        assert!(!rt.scene_is_being_walked());
        assert_eq!(rt.destroy_node(doomed), Some(()));
    }

    /// The Lock band cannot be hidden while a lock is held.
    ///
    /// It is what a lock is made of — the opaque fill and every lock surface
    /// live in it — so disabling it uncovered the desktop while
    /// `is_session_locked()` still said `true` and the rest of the crate went
    /// on behaving as locked. A screen showing a session the compositor
    /// believes is locked. `set_node_enabled` accepts protected nodes by
    /// design (the module doc says so), which is exactly why this one band
    /// needs the explicit refusal.
    #[test]
    fn the_lock_band_cannot_be_hidden_while_the_session_is_locked() {
        let rt = headless_runtime();
        let band = rt.band_node(Band::Lock).expect("lock band");

        // Unlocked, the band is ordinary: an empty band nothing is relying on.
        assert_eq!(rt.set_node_enabled(band, false), Some(()));
        assert_eq!(rt.set_node_enabled(band, true), Some(()));

        rt.inner.session_locked.set(true);
        assert_eq!(
            rt.set_node_enabled(band, false),
            None,
            "hiding the lock band while locked uncovers the desktop"
        );
        // Re-enabling is always allowed — it can only ever make the lock more
        // complete, never less.
        assert_eq!(rt.set_node_enabled(band, true), Some(()));

        // Only this band. Every other band stays hideable under a lock.
        for other in [Band::Background, Band::Bottom, Band::Top, Band::Overlay] {
            let node = rt.band_node(other).expect("band");
            assert_eq!(
                rt.set_node_enabled(node, false),
                Some(()),
                "{other:?} is not the lock band"
            );
            assert_eq!(rt.set_node_enabled(node, true), Some(()));
        }
        rt.inner.session_locked.set(false);
    }

    /// Appearance setters reach a foreign node; placement ones still do not.
    ///
    /// The Owned-only rule exists for this crate's placement bookkeeping,
    /// which `set_toplevel_position` and `raise_toplevel` maintain. Applying
    /// it to the appearance setters made fading a client's window — from the
    /// very `NodeId` `node_at` hands back — return `None` with no diagnostic,
    /// and contradicted each of those methods' own documented `None` cases.
    #[test]
    fn appearance_setters_reach_a_foreign_node_but_placement_does_not() {
        let rt = headless_runtime();
        let band = rt.band_node(Band::Overlay).expect("band");
        let owned = rt.create_scene_buffer(band, None).expect("buffer node");

        // Re-observing an owned node through the child walk yields the same
        // id, so to get a genuinely foreign one this forces the origin
        // directly — the same state `node_at` on a client surface produces.
        rt.inner
            .nodes
            .borrow_mut()
            .get_mut(&owned)
            .expect("row")
            .origin = NodeOrigin::Foreign;

        assert_eq!(
            rt.set_scene_buffer_opacity(owned, 0.5),
            Some(()),
            "fading a client's window is the ordinary case"
        );
        assert_eq!(
            rt.set_scene_buffer_filter(owned, FilterMode::Nearest),
            Some(())
        );
        assert_eq!(
            rt.set_scene_buffer_transform(owned, Transform::R90),
            Some(())
        );
        assert_eq!(rt.set_scene_buffer_dest_size(owned, 8, 8), Some(()));

        assert_eq!(
            rt.set_node_position(owned, 1, 1),
            None,
            "placement still goes through the toplevel and layer APIs"
        );
    }

    /// The reverse pairing: the published `remove_rect` path must leave no
    /// node row behind either, or `node_children` would keep reporting a node
    /// wlroots has freed.
    #[test]
    fn remove_rect_purges_the_node_row() {
        let rt = headless_runtime();
        let rect = rt
            .add_rect_in_band(Band::Top, 8, 8, [0.0, 1.0, 0.0, 1.0])
            .expect("rect");
        let node = rt.rect_node(rect).expect("node id");
        assert_eq!(rt.remove_rect(rect), Some(()));
        assert_eq!(rt.node_kind(node), None);
        assert_eq!(rt.node_parent(node), None);
    }

    /// A rect moved out of a toplevel's tree must stop being purged with that
    /// toplevel, and one moved in must start — the parent tracking the frozen
    /// `RectId` table still carries is recomputed by `reparent_node`, and this
    /// is the property that recomputation exists for.
    #[test]
    fn reparenting_a_legacy_rect_moves_it_between_purge_classes() {
        let rt = headless_runtime();
        let toplevel_band = rt.toplevel_band_ptr().expect("toplevel band");
        // SAFETY: `toplevel_band` is a live tree owned by `rt`'s own scene.
        let tree =
            NonNull::new(unsafe { sys::wlr_scene_tree_create(toplevel_band.as_ptr()) }).unwrap();
        let toplevel = ToplevelId(next_id());
        rt.record_toplevel(toplevel, NonNull::<sys::wlr_xdg_toplevel>::dangling(), tree);

        let rect = rt
            .add_rect_in_band(Band::Toplevel, 4, 4, [1.0, 1.0, 1.0, 1.0])
            .expect("rect");
        let node = rt.rect_node(rect).expect("node id");
        assert_eq!(
            rt.inner.rects.borrow()[&rect].parent,
            RectParent::Band(Band::Toplevel)
        );

        // SAFETY: `tree` is the live tree recorded just above.
        let into =
            unsafe { rt.ensure_node_id(&raw mut (*tree.as_ptr()).node, NodeOrigin::Foreign) }
                .expect("the toplevel's tree gets an id on demand");
        assert_eq!(rt.reparent_node(node, into), Some(()));
        assert_eq!(
            rt.inner.rects.borrow()[&rect].parent,
            RectParent::Toplevel(toplevel),
            "a rect moved into a toplevel's tree must now die with it"
        );

        // And back out again.
        let band = rt.band_node(Band::Toplevel).expect("band id");
        assert_eq!(rt.reparent_node(node, band), Some(()));
        assert_eq!(
            rt.inner.rects.borrow()[&rect].parent,
            RectParent::Band(Band::Toplevel),
            "and one moved back out must stop dying with it"
        );
        assert_eq!(rt.remove_rect(rect), Some(()));
    }

    /// Restacking is refused mid-walk, like destroying already was.
    ///
    /// `for_each_buffer`'s doc forbids freeing *or moving* a node during the
    /// walk, but only the destroys enforced it. wlroots iterates with
    /// `wl_list_for_each` rather than the `_safe` variant, so a node unlinked
    /// and reinserted elsewhere leaves the walk following `link.next` into
    /// where it used to be — the iteration stops early and silently, which is
    /// the failure mode that does not announce itself.
    #[test]
    fn restacking_during_a_buffer_walk_is_refused() {
        let rt = headless_runtime();
        let a = rt
            .add_rect_in_band(Band::Overlay, 4, 4, [1.0, 0.0, 0.0, 1.0])
            .expect("rect a");
        let b = rt
            .add_rect_in_band(Band::Overlay, 4, 4, [0.0, 1.0, 0.0, 1.0])
            .expect("rect b");
        let node_a = rt.rect_node(a).expect("node a");
        let node_b = rt.rect_node(b).expect("node b");
        // A rect is not a buffer node, so a walk over rects alone visits
        // nothing and the assertion below would hold vacuously. Give the walk
        // something to actually visit.
        let pixels = vec![0xffu8; 4 * 4 * 4];
        let buffer = rt.add_buffer(4, 4, &pixels).expect("pixel buffer");

        let mut refusals = Vec::new();
        let root = rt.scene_root_node().expect("scene root");
        rt.for_each_buffer(root, |_, _, _| {
            refusals.push(rt.raise_node_to_top(node_a));
            refusals.push(rt.lower_node_to_bottom(node_a));
            refusals.push(rt.place_node_above(node_a, node_b));
            refusals.push(rt.lower_rect_to_bottom(a));
        })
        .expect("the scene root is walkable");

        assert!(
            !refusals.is_empty(),
            "the walk must have visited the buffer node, or this proves nothing"
        );
        assert!(
            refusals.iter().all(|r| r.is_none()),
            "no restack may succeed inside a walk: {refusals:?}"
        );

        // Outside the walk they work again, so the refusal is scoped.
        assert_eq!(rt.raise_node_to_top(node_a), Some(()));
        assert_eq!(rt.lower_rect_to_bottom(a), Some(()));
        assert_eq!(rt.remove_buffer(buffer), Some(()));
    }

    /// A foreign node refuses the mutators, as `scene`'s module doc promises.
    ///
    /// It promised it for protected *and* foreign nodes, but the check tested
    /// only for protected — so this half was documented and not enforced, and
    /// `set_node_position` could move a toplevel's own tree while
    /// `set_toplevel_position` and `raise_toplevel` went on believing they
    /// knew where it was.
    ///
    /// `set_node_enabled` is deliberately still allowed: its own doc grants it
    /// on every origin, and hiding a node breaks no bookkeeping.
    #[test]
    fn a_foreign_node_refuses_the_mutators_but_still_hides() {
        let rt = headless_runtime();
        let band = rt.band_ptr(Band::Toplevel).expect("band");
        // SAFETY: the band tree is this runtime's own and lives for the process.
        let tree = unsafe { sys::wlr_scene_tree_create(band.as_ptr()) };
        let tree = NonNull::new(tree).expect("tree");

        // SAFETY: `tree` was just created and is live.
        let foreign =
            unsafe { rt.ensure_node_id(&raw mut (*tree.as_ptr()).node, NodeOrigin::Foreign) }
                .expect("a foreign node gets an id on demand");

        assert_eq!(
            rt.set_node_position(foreign, 5, 5),
            None,
            "moving a node this crate does not own must be refused"
        );
        assert_eq!(
            rt.set_node_enabled(foreign, false),
            Some(()),
            "but hiding it is allowed on every origin"
        );
    }

    /// A runtime with a real seat — and so a real `wlr_cursor` and
    /// `wlr_xcursor_manager` — for the cursor-shape tests below. The
    /// `Display` is leaked for exactly the reason `headless_runtime`'s own
    /// doc gives.
    fn seated_runtime() -> Runtime {
        headless_env();
        let display: &'static crate::Display =
            Box::leak(Box::new(crate::Display::new().expect("display")));
        let rt = Runtime::new().expect("runtime");
        rt.create_seat(display, "seat0").expect("seat");
        rt
    }

    /// The bug this release exists for: before 0.20.26 `ensure_cursor_image`
    /// pushed `left_ptr` unconditionally from all three pointer callbacks,
    /// so a shape a `cursor-shape-v1` client named survived exactly until
    /// that client's next motion event. It must now leave a named shape
    /// alone — its own doc always claimed it only set an image "whenever the
    /// cursor has none".
    ///
    /// Asserted through `applied_cursor` rather than by reading the image
    /// off the cursor: `wlr_cursor.state` is `WLR_PRIVATE`, so what was last
    /// handed to `wlr_cursor_set_xcursor` is the only observable proxy.
    #[test]
    fn ensure_cursor_image_does_not_stomp_a_named_shape() {
        let rt = seated_runtime();
        rt.set_cursor_shape(CursorShape::Text);
        assert_eq!(rt.cursor_shape(), Some(CursorShape::Text));
        assert_eq!(rt.applied_cursor(), Some(Some(CursorShape::Text)));

        // Three motions' worth of the call every pointer callback makes.
        rt.ensure_cursor_image();
        rt.ensure_cursor_image();
        rt.ensure_cursor_image();

        assert_eq!(
            rt.cursor_shape(),
            Some(CursorShape::Text),
            "a named shape must survive pointer motion"
        );
        assert_eq!(
            rt.applied_cursor(),
            Some(Some(CursorShape::Text)),
            "ensure_cursor_image must not push left_ptr over a named shape"
        );
    }

    /// With nothing named, `ensure_cursor_image` still does what it always
    /// did: give the cursor the default `left_ptr` image.
    #[test]
    fn ensure_cursor_image_still_applies_the_default_when_nothing_is_named() {
        let rt = seated_runtime();
        assert_eq!(rt.cursor_shape(), None);
        rt.ensure_cursor_image();
        assert_eq!(rt.applied_cursor(), Some(None));
    }

    /// A failed theme load must be retried on the next pointer event.
    ///
    /// The trap this pins: with nothing named, `applied_cursor` reaches its
    /// steady state (`Some(None)`) after one call, so an `apply_cursor` that
    /// short-circuited on it *before* attempting the load would never retry
    /// — and a machine with no cursor theme at the moment the compositor
    /// starts would keep a blank cursor for the rest of the session.
    ///
    /// The two `Cell`s are set by hand to exactly the state a first call
    /// whose load failed leaves behind (`cursor_image_loaded == false`,
    /// `applied_cursor == Some(None)`); a second `ensure_cursor_image` must
    /// then still reach the load and latch it.
    ///
    /// Self-gating: the assertion only means anything where a cursor theme
    /// is actually installed, so a control runtime establishes that first
    /// and the test returns early where it is not (a bare CI container).
    #[test]
    fn a_failed_theme_load_is_retried_on_the_next_pointer_event() {
        let control = seated_runtime();
        control.ensure_cursor_image();
        if !control.inner.cursor_image_loaded.get() {
            // No cursor theme on this machine: `wlr_xcursor_manager_load`
            // cannot succeed, so "the retry latched it" is unobservable.
            return;
        }

        let rt = seated_runtime();
        rt.ensure_cursor_image();
        assert_eq!(rt.applied_cursor(), Some(None));

        // Rewind to "the first call's load failed".
        rt.inner.cursor_image_loaded.set(false);
        assert_eq!(
            rt.applied_cursor(),
            Some(None),
            "the short-circuit's condition is satisfied, which is the point"
        );

        rt.ensure_cursor_image();
        assert!(
            rt.inner.cursor_image_loaded.get(),
            "a second pointer event must re-attempt the theme load even \
             though the image it would apply is already applied"
        );
    }

    /// What `backend.rs`'s `on_pointer_focus_change` calls: the named shape
    /// goes away and the default image comes back, so a consumer never has
    /// to hit-test its own geometry to decide when a client's cursor stops
    /// applying.
    #[test]
    fn resetting_the_named_cursor_restores_the_default_image() {
        let rt = seated_runtime();
        rt.set_cursor_shape(CursorShape::Text);
        rt.reset_named_cursor();
        assert_eq!(rt.cursor_shape(), None);
        assert_eq!(rt.applied_cursor(), Some(None));
        // And a later motion keeps it there rather than resurrecting Text.
        rt.ensure_cursor_image();
        assert_eq!(rt.applied_cursor(), Some(None));
    }

    /// Naming the shape that is already in force must not reach wlroots at
    /// all. Observed by planting a different value in `applied_cursor` and
    /// checking the second `set_cursor_shape` leaves it there: an
    /// un-short-circuited call would overwrite it with `Some(Some(Text))`.
    #[test]
    fn naming_the_shape_already_in_force_is_a_no_op() {
        let rt = seated_runtime();
        rt.set_cursor_shape(CursorShape::Text);
        assert_eq!(rt.applied_cursor(), Some(Some(CursorShape::Text)));

        rt.inner.applied_cursor.set(Some(None));
        rt.set_cursor_shape(CursorShape::Text);
        assert_eq!(
            rt.applied_cursor(),
            Some(None),
            "the equality short-circuit must skip the wlroots call entirely"
        );
    }

    /// `CursorShape::Default` is the "un-name it" value: it clears the named
    /// cursor rather than becoming it, so a consumer that never names
    /// anything else sees exactly the pre-0.20.26 behaviour.
    #[test]
    fn naming_the_default_shape_clears_the_named_cursor() {
        let rt = seated_runtime();
        rt.set_cursor_shape(CursorShape::Text);
        rt.set_cursor_shape(CursorShape::Default);
        assert_eq!(rt.cursor_shape(), None);
        assert_eq!(rt.applied_cursor(), Some(None));
    }

    /// `CursorShape::Pointer` is a distinct shape (`cursor-shape-v1`'s
    /// "pointer that indicates a link or another interactive element" — the
    /// hand cursor), not an alias for `CursorShape::Default`, so unlike
    /// `Default` it is a real named shape and must persist.
    #[test]
    fn the_explicit_pointer_shape_is_named_rather_than_clearing() {
        let rt = seated_runtime();
        rt.set_cursor_shape(CursorShape::Pointer);
        assert_eq!(rt.cursor_shape(), Some(CursorShape::Pointer));
        rt.ensure_cursor_image();
        assert_eq!(rt.applied_cursor(), Some(Some(CursorShape::Pointer)));
    }

    /// Record a popup with a heap-allocated `wlr_xdg_popup`/`wlr_xdg_surface`
    /// pair, and return the id.
    ///
    /// `alloc_zeroed` behind a raw pointer and deliberately leaked, for the
    /// reasons `record_toplevel_with_surface` above already documents in full:
    /// the structs embed `wl_listener`s whose function pointers are UB to
    /// materialise as zero, and these tests never enter wlroots' lifecycle so
    /// nothing frees them.
    fn record_scratch_popup(rt: &Runtime, parent: PopupParent, initialized: bool) -> PopupId {
        use std::alloc::{Layout, alloc_zeroed};

        let id = PopupId(next_id());
        // SAFETY: both layouts are non-zero-sized, so `alloc_zeroed` returns
        // either null (checked) or a suitably aligned zeroed allocation.
        let base = unsafe { alloc_zeroed(Layout::new::<sys::wlr_xdg_surface>()) }
            .cast::<sys::wlr_xdg_surface>();
        assert!(!base.is_null(), "allocation failed");
        let popup = unsafe { alloc_zeroed(Layout::new::<sys::wlr_xdg_popup>()) }
            .cast::<sys::wlr_xdg_popup>();
        assert!(!popup.is_null(), "allocation failed");
        // A zeroed `wlr_surface` with a null `role` is a real, if roleless,
        // surface — `wlr_xdg_surface_try_from_wlr_surface` reads `role` and
        // returns `NULL` when it does not match, so this stops
        // `wlr_xdg_popup_get_toplevel_coords`'s parent walk cleanly instead of
        // dereferencing a null `parent`, which `wlr_xdg_popup_unconstrain_from_box`
        // (called by `configure_popup` even on an uninitialized surface) reaches
        // into unconditionally — a real popup always has `parent` set at
        // creation, before `initialized` is ever true, so this only patches the
        // scratch fixture up to that same invariant.
        let parent_surface =
            unsafe { alloc_zeroed(Layout::new::<sys::wlr_surface>()) }.cast::<sys::wlr_surface>();
        assert!(!parent_surface.is_null(), "allocation failed");
        // SAFETY: both allocations are freshly zeroed and exclusively owned.
        unsafe {
            (*base).initialized = initialized;
            (*popup).base = base;
            (*popup).parent = parent_surface;
        }
        rt.record_popup(
            id,
            NonNull::new(popup).expect("allocation succeeded"),
            NonNull::<sys::wlr_scene_tree>::dangling(),
            parent,
        );
        id
    }

    #[test]
    fn an_unknown_popup_id_misses_rather_than_dereferencing() {
        let rt = Runtime::new().expect("runtime");
        let dead = PopupId::dangling_for_test();
        assert!(rt.popup(dead).is_none());
        assert_eq!(rt.popup_parent(dead), None);
        assert!(rt.popup_chain(PopupParent::Popup(dead)).is_empty());
        assert!(rt.popups_of(PopupParent::Popup(dead)).is_empty());
    }

    /// Direct children only, in creation order — which is also the z-order
    /// tiebreak the compositor's own stack relies on.
    #[test]
    fn popups_of_lists_direct_children_in_creation_order() {
        let rt = Runtime::new().expect("runtime");
        let window = PopupParent::Toplevel(ToplevelId::dangling_for_test());
        let a = record_scratch_popup(&rt, window, false);
        let b = record_scratch_popup(&rt, window, false);
        let nested = record_scratch_popup(&rt, PopupParent::Popup(a), false);

        assert_eq!(rt.popups_of(window), vec![a, b]);
        assert_eq!(rt.popups_of(PopupParent::Popup(a)), vec![nested]);
        assert!(rt.popups_of(PopupParent::Popup(b)).is_empty());
    }

    /// The whole subtree, deepest last — the order a caller iterates to paint,
    /// and the *reverse* of the order it must destroy in.
    #[test]
    fn popup_chain_walks_the_whole_subtree_deepest_last() {
        let rt = Runtime::new().expect("runtime");
        let window = PopupParent::Toplevel(ToplevelId::dangling_for_test());
        let menu = record_scratch_popup(&rt, window, false);
        let submenu = record_scratch_popup(&rt, PopupParent::Popup(menu), false);
        let subsub = record_scratch_popup(&rt, PopupParent::Popup(submenu), false);
        let sibling = record_scratch_popup(&rt, window, false);

        let chain = rt.popup_chain(window);
        assert_eq!(chain.len(), 4, "every popup under the window: {chain:?}");
        assert_eq!(
            chain[0], menu,
            "a direct child comes before its own children"
        );
        assert!(
            chain.iter().position(|p| *p == submenu) < chain.iter().position(|p| *p == subsub),
            "a parent must precede its child: {chain:?}"
        );
        assert!(chain.contains(&sibling));

        assert_eq!(
            rt.popup_chain(PopupParent::Popup(menu)),
            vec![submenu, subsub]
        );
        assert!(rt.popup_chain(PopupParent::Popup(subsub)).is_empty());
    }

    /// `root` walks `Popup(_)` links down to the window or layer at the bottom.
    #[test]
    fn a_popup_parent_resolves_to_the_window_or_layer_at_the_root() {
        let rt = Runtime::new().expect("runtime");
        let window = PopupParent::Toplevel(ToplevelId::dangling_for_test());
        let menu = record_scratch_popup(&rt, window, false);
        let submenu = record_scratch_popup(&rt, PopupParent::Popup(menu), false);

        assert_eq!(window.root(&rt), Some(window));
        assert_eq!(PopupParent::Popup(menu).root(&rt), Some(window));
        assert_eq!(PopupParent::Popup(submenu).root(&rt), Some(window));

        let layer = PopupParent::Layer(LayerSurfaceId::dangling_for_test());
        let panel_menu = record_scratch_popup(&rt, layer, false);
        assert_eq!(PopupParent::Popup(panel_menu).root(&rt), Some(layer));
    }

    /// A link already dead resolves to nothing rather than to a wrong window —
    /// the by-id contract every other accessor in this crate carries.
    #[test]
    fn a_root_walk_through_a_dead_link_is_none() {
        let rt = Runtime::new().expect("runtime");
        assert_eq!(
            PopupParent::Popup(PopupId::dangling_for_test()).root(&rt),
            None
        );
    }

    /// A cycle cannot arise from wlroots — a popup's parent is fixed at
    /// creation and a client cannot re-parent one — but `root` must terminate
    /// on a corrupted table anyway, because a hang in a compositor's input path
    /// is indistinguishable from a freeze to the user. The depth cap is what
    /// guarantees it.
    #[test]
    fn a_root_walk_terminates_even_on_a_cyclic_table() {
        let rt = Runtime::new().expect("runtime");
        let a = record_scratch_popup(
            &rt,
            PopupParent::Toplevel(ToplevelId::dangling_for_test()),
            false,
        );
        let b = record_scratch_popup(&rt, PopupParent::Popup(a), false);
        // Forge the cycle a -> b -> a directly in the table.
        rt.inner
            .popups
            .borrow_mut()
            .get_mut(&a)
            .expect("recorded")
            .parent = PopupParent::Popup(b);

        assert_eq!(PopupParent::Popup(a).root(&rt), None, "cap, not hang");
    }

    /// A chain walk over the same cyclic table must terminate too, and must not
    /// return the same id twice — a caller destroying what it returns would
    /// double-destroy.
    #[test]
    fn a_chain_walk_terminates_and_never_repeats_an_id_on_a_cyclic_table() {
        let rt = Runtime::new().expect("runtime");
        let window = PopupParent::Toplevel(ToplevelId::dangling_for_test());
        let a = record_scratch_popup(&rt, window, false);
        let b = record_scratch_popup(&rt, PopupParent::Popup(a), false);
        rt.inner
            .popups
            .borrow_mut()
            .get_mut(&a)
            .expect("recorded")
            .parent = PopupParent::Popup(b);

        let chain = rt.popup_chain(PopupParent::Popup(a));
        let mut seen = chain.clone();
        seen.sort_unstable_by_key(|p| p.0);
        seen.dedup();
        assert_eq!(seen.len(), chain.len(), "no id twice: {chain:?}");
    }

    /// Forgetting one popup leaves its siblings and its parent alone, and does
    /// **not** touch the scene tree: a popup's tree is a child of its parent's,
    /// and wlroots frees a tree's children recursively — the double free
    /// `forget_toplevel`'s own comment spells out.
    #[test]
    fn forgetting_a_popup_removes_only_that_row() {
        let rt = Runtime::new().expect("runtime");
        let window = PopupParent::Toplevel(ToplevelId::dangling_for_test());
        let a = record_scratch_popup(&rt, window, false);
        let b = record_scratch_popup(&rt, window, false);

        rt.forget_popup(a);
        assert!(rt.popup(a).is_none());
        assert_eq!(rt.popups_of(window), vec![b]);
        assert_eq!(rt.popup_parent(b), Some(window));
    }

    /// `clear_popups` is the run-granularity purge `run_inner` calls when
    /// `run_all` returns, mirroring `clear_toplevels`: popup ids are only
    /// meaningful for the call that announced them, because the per-popup
    /// destroy listener that would otherwise remove a stale row is torn down
    /// with that call's `Session`.
    #[test]
    fn clear_popups_empties_the_table() {
        let rt = Runtime::new().expect("runtime");
        let window = PopupParent::Toplevel(ToplevelId::dangling_for_test());
        let a = record_scratch_popup(&rt, window, false);
        record_scratch_popup(&rt, PopupParent::Popup(a), false);

        rt.clear_popups();
        assert!(rt.popups_of(window).is_empty());
        assert!(rt.popup(a).is_none());
    }

    /// A popup whose surface is not yet `initialized` cannot be configured:
    /// `Popup::send_configure` skips the call (see its own doc — this
    /// distribution's wlroots asserts on that flag and aborts), so
    /// `configure_popup` must report `false` rather than claiming success. The
    /// unconstrain half still runs, which is harmless and is what leaves
    /// `scheduled.geometry` correct for the configure the initial commit will
    /// trigger moments later.
    #[test]
    fn configuring_an_uninitialized_popup_reports_false() {
        let rt = Runtime::new().expect("runtime");
        let window = PopupParent::Toplevel(ToplevelId::dangling_for_test());
        let id = record_scratch_popup(&rt, window, false);
        assert!(!rt.configure_popup(id, &Box2D::new(0, 0, 800, 600)));
    }

    #[test]
    fn configuring_an_unknown_popup_reports_false_rather_than_dereferencing() {
        let rt = Runtime::new().expect("runtime");
        assert!(!rt.configure_popup(PopupId::dangling_for_test(), &Box2D::new(0, 0, 800, 600)));
        assert_eq!(rt.popup_position(PopupId::dangling_for_test()), None);
        assert!(!rt.popup_is_grabbing(PopupId::dangling_for_test()));
    }

    /// `dismiss_popup` returns how many popups it destroyed, and destroys the
    /// whole subtree under `id` as well as `id` itself.
    ///
    /// **Cannot run against this crate's scratch popups.** An earlier draft of
    /// this test assumed `Popup::destroy` on an unwired scratch popup was a
    /// harmless no-op ("nothing is actually freed and no destroy signal
    /// fires"); it is not. Traced by disassembling `libwlroots-0.20.so`:
    /// `wlr_xdg_popup_destroy` walks `base->popups` (a real `wl_list`, which a
    /// zeroed scratch struct does not have — the first crash this hits), then
    /// unconditionally calls `wl_resource_post_event(popup->resource, ...)` to
    /// send `xdg_popup.popup_done`, then `destroy_xdg_popup` goes on to touch
    /// `base->surface` and `wl_resource_set_user_data(popup->resource, NULL)`.
    /// Every one of those needs a genuine, wire-created object: `wl_resource`
    /// is opaque even to this crate's own bindings (no `struct` definition to
    /// fake), and a `wlr_surface` can only be produced by wlroots' own
    /// `wlr_compositor` global answering a real client's `wl_surface` request
    /// — there is no public `wlr_surface_create`. None of that is
    /// constructible from a bare unit test.
    ///
    /// So this is `#[ignore]`d rather than passing or being deleted: the
    /// *count and order* logic it describes is real and still needs coverage,
    /// but only the harness-driven `compositor/tests/popups.rs` (P2), which
    /// runs a genuine client against a genuine xdg-shell, can actually exercise
    /// `Popup::destroy`'s FFI without crashing the test process.
    #[test]
    #[ignore = "needs a live wl_resource/wlr_surface chain a scratch popup can't fake; see compositor/tests/popups.rs (P2)"]
    fn dismissing_a_popup_counts_itself_and_its_whole_subtree() {
        let rt = Runtime::new().expect("runtime");
        let window = PopupParent::Toplevel(ToplevelId::dangling_for_test());
        let menu = record_scratch_popup(&rt, window, false);
        let submenu = record_scratch_popup(&rt, PopupParent::Popup(menu), false);
        record_scratch_popup(&rt, PopupParent::Popup(submenu), false);
        let sibling = record_scratch_popup(&rt, window, false);

        assert_eq!(
            rt.dismiss_popup(menu),
            3,
            "the menu and its two descendants"
        );
        assert_eq!(
            rt.popups_of(window),
            vec![sibling],
            "a sibling chain is untouched"
        );
    }

    #[test]
    fn dismissing_an_unknown_popup_destroys_nothing() {
        let rt = Runtime::new().expect("runtime");
        assert_eq!(rt.dismiss_popup(PopupId::dangling_for_test()), 0);
        assert_eq!(
            rt.dismiss_popups_of(PopupParent::Popup(PopupId::dangling_for_test())),
            0
        );
    }

    /// See [`dismissing_a_popup_counts_itself_and_its_whole_subtree`]'s doc:
    /// same real-FFI-destroy hazard, since this also bottoms out in
    /// `dismiss_popup`.
    #[test]
    #[ignore = "needs a live wl_resource/wlr_surface chain a scratch popup can't fake; see compositor/tests/popups.rs (P2)"]
    fn dismissing_a_parents_popups_covers_every_chain_under_it() {
        let rt = Runtime::new().expect("runtime");
        let window = PopupParent::Toplevel(ToplevelId::dangling_for_test());
        let a = record_scratch_popup(&rt, window, false);
        record_scratch_popup(&rt, PopupParent::Popup(a), false);
        record_scratch_popup(&rt, window, false);

        assert_eq!(rt.dismiss_popups_of(window), 3);
        assert!(rt.popups_of(window).is_empty());
    }

    /// The destruction order is deepest-first, which is what xdg-shell requires
    /// (`xdg_popup.destroy` on a popup with live children is a protocol error).
    /// The order is observable here because `dismiss_popup` forgets each row as
    /// it goes: a shallow-first implementation would find the deeper rows
    /// already unreachable and under-count.
    ///
    /// See [`dismissing_a_popup_counts_itself_and_its_whole_subtree`]'s doc:
    /// same real-FFI-destroy hazard.
    #[test]
    #[ignore = "needs a live wl_resource/wlr_surface chain a scratch popup can't fake; see compositor/tests/popups.rs (P2)"]
    fn dismissal_is_deepest_first() {
        let rt = Runtime::new().expect("runtime");
        let window = PopupParent::Toplevel(ToplevelId::dangling_for_test());
        let a = record_scratch_popup(&rt, window, false);
        let b = record_scratch_popup(&rt, PopupParent::Popup(a), false);
        let c = record_scratch_popup(&rt, PopupParent::Popup(b), false);

        let order = rt.popup_chain(PopupParent::Popup(a));
        assert_eq!(order, vec![b, c], "chain order is shallow-first…");
        assert_eq!(
            rt.dismiss_popup(a),
            3,
            "…and dismissal reverses it, so every row is still present when its \
             own destroy runs"
        );
    }

    /// With no seat created there is no grab to observe, and asking must be a
    /// plain `false` rather than a null dereference — a compositor calls this
    /// from `sync_seat_focus`, which runs before a seat exists during startup.
    #[test]
    fn a_runtime_without_a_seat_has_no_explicit_grab() {
        let rt = Runtime::new().expect("runtime");
        assert!(!rt.seat_has_explicit_grab());
    }
}
