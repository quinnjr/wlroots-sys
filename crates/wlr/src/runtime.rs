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
use crate::decoration::DecorationEntry;
use crate::id::{SourceId, next_id};
use crate::scene::RectId;
use crate::{Backend, BufferId, Display, Error, Interest, Output, Result, ToplevelId, sys};

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
}

#[derive(Clone, Copy)]
pub(crate) struct ToplevelEntry {
    pub(crate) raw: NonNull<sys::wlr_xdg_toplevel>,
    pub(crate) tree: NonNull<sys::wlr_scene_tree>,
}

/// A live scene rect: the node wlroots created for it, and which toplevel
/// (if any) it is parented into.
#[derive(Clone, Copy)]
pub(crate) struct RectEntry {
    pub(crate) raw: NonNull<sys::wlr_scene_rect>,
    /// `Some(id)` for a rect [`Runtime::add_rect_in_toplevel`] created,
    /// parented into that toplevel's own scene tree. `None` for a root
    /// rect [`Runtime::add_rect`] created, parented into the scene's root
    /// tree instead.
    ///
    /// Read by [`Runtime::forget_toplevel`] to purge every rect that dies
    /// along with a destroyed toplevel's tree, without double-destroying
    /// the node wlroots is about to free recursively.
    pub(crate) parent: Option<ToplevelId>,
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
/// `init_graphics` stored, live or not. **Nothing in this crate enforces
/// this today.** Reachability is narrow in practice — a handle to either
/// type only exists inside a handler call, so violating this means a
/// consumer deliberately kept a `Runtime` clone somewhere the `Display`
/// does not reach and used it afterward — but the obligation is real, is
/// not checked, and is the caller's to keep.
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
                toplevels: RefCell::new(HashMap::new()),
                decorations: RefCell::new(HashMap::new()),
                tree_to_toplevel: RefCell::new(HashMap::new()),
                seat: RefCell::new(None),
                cursor: RefCell::new(None),
                xcursor: RefCell::new(None),
                cursor_image_loaded: std::cell::Cell::new(false),
                keyboards: RefCell::new(Vec::new()),
                pointers: RefCell::new(Vec::new()),
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
    /// Specifically: `wlr_scene_create`, `wlr_output_layout_create`,
    /// `wlr_scene_attach_output_layout`, `wlr_renderer_autocreate`,
    /// `wlr_allocator_autocreate`, `wlr_renderer_init_wl_display` (which is
    /// what advertises `wl_shm` and the dmabuf formats), `wlr_compositor_create`
    /// at version 6, `wlr_subcompositor_create` and
    /// `wlr_data_device_manager_create`.
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

            Graphics {
                scene,
                layout,
                scene_layout,
                renderer,
                allocator,
            }
        };
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

    /// Add a solid-colour rect to the scene, at the root, in RGBA where each
    /// channel is 0.0–1.0 and the colour is premultiplied.
    ///
    /// Positioned at (0, 0) until [`set_rect_position`](Runtime::set_rect_position)
    /// says otherwise, and on top of everything already in the scene — call
    /// [`lower_rect_to_bottom`](Runtime::lower_rect_to_bottom) for a
    /// background.
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
        self.inner
            .rects
            .borrow_mut()
            .insert(id, RectEntry { raw, parent: None });
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
                parent: Some(toplevel),
            },
        );
        Some(id)
    }

    /// Destroy a rect's scene node, whether it is a root rect from
    /// [`add_rect`](Runtime::add_rect) or one parented into a toplevel via
    /// [`add_rect_in_toplevel`](Runtime::add_rect_in_toplevel).
    ///
    /// `None` if this runtime never issued `rect`, including a rect already
    /// removed (by this call or by its parent toplevel's own teardown) —
    /// double-removal misses cleanly rather than double-destroying the
    /// node.
    pub fn remove_rect(&self, rect: RectId) -> Option<()> {
        let entry = self.inner.rects.borrow_mut().remove(&rect)?;
        // SAFETY: `entry.raw` came from `add_rect`/`add_rect_in_toplevel`
        // and the table entry naming it is only ever removed once — by
        // this call, or (without a matching destroy; see their own
        // comments) by `forget_toplevel`'s per-toplevel purge or
        // `clear_toplevels`' run-granularity purge — so the node has not
        // been destroyed yet.
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

    /// Add a scene node showing owned RGBA8888 pixels (bytes R, G, B, A per
    /// pixel, row-major, stride = `width * 4`), at the root, at (0, 0) until
    /// [`set_buffer_position`](Runtime::set_buffer_position) says otherwise
    /// and on top of everything already in the scene — call
    /// [`lower_buffer_to_bottom`](Runtime::lower_buffer_to_bottom) for a
    /// background.
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
        if !crate::buffer::validate_pixels(width, height, rgba.len()) {
            return None;
        }
        let entry = self.toplevel_entry(toplevel)?;
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
        if !crate::buffer::validate_pixels(width, height, rgba.len()) {
            return None;
        }
        let node = self.buffer_ptr(buffer)?;
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
            .retain(|_, entry| entry.parent != Some(id));
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
            .retain(|_, entry| entry.parent.is_none());
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
    pub(crate) fn take_staged_decoration_mode(&self, id: ToplevelId) -> Option<bool> {
        self.inner
            .decorations
            .borrow()
            .get(&id)
            .and_then(|e| e.staged.take())
    }

    /// Whether `id` has a decoration, and if so, the client's current
    /// stated preference for it — read fresh from
    /// `wlr_xdg_toplevel_decoration_v1::requested_mode` through
    /// [`crate::decoration::client_side_preference`], not cached, since the
    /// only caller (`on_surface_commit`'s "nothing has ever asked for this
    /// decoration" path) wants whatever is true *now*.
    pub(crate) fn decoration_requested_preference(&self, id: ToplevelId) -> Option<Option<bool>> {
        let raw = self.decoration_ptr(id)?;
        // SAFETY: a present `decorations` entry names a decoration still
        // linked into the table — removed synchronously, before wlroots
        // frees it, by whichever of `forget_decoration`/`forget_toplevel`
        // runs first (see `RuntimeInner::decorations`'s own doc) — so `raw`
        // is live.
        let requested = unsafe { (*raw.as_ptr()).requested_mode };
        Some(crate::decoration::client_side_preference(requested))
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

    /// Raise the toplevel above every sibling in the scene. `None` for an
    /// unknown or stale id; see `set_toplevel_size`'s doc.
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
    /// to answer the client's request. `server_side` is the mode to send —
    /// `true` for `WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE`, `false`
    /// for `..._CLIENT_SIDE` — not a toggle. Calling this marks the request
    /// currently in flight as answered, so the dispatch layer's server-side
    /// default (see `request_decoration_mode`'s own doc) does not also fire
    /// once the handler returns.
    ///
    /// **Staged, not always sent.** wlroots asserts `surface->initialized`
    /// inside `wlr_xdg_surface_schedule_configure`, which
    /// `wlr_xdg_toplevel_decoration_v1_set_mode` calls internally, and that
    /// flag only flips true during the toplevel's first role commit — which
    /// has not necessarily happened yet: the normal client sequence calls
    /// `set_mode` (firing `request_decoration_mode`) *before* its initial
    /// `wl_surface.commit`. So this method sends immediately only if the
    /// surface is already initialized; otherwise it records `server_side`
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
    pub fn set_decoration_mode(&self, id: ToplevelId, server_side: bool) -> Option<()> {
        let raw = self.decoration_ptr(id)?;
        let entry = self.toplevel_entry(id)?;
        // SAFETY: a present `decorations` entry implies a live toplevel too
        // — both halves of `RuntimeInner::decorations`' purge remove the
        // decoration entry the moment either object dies — and a live
        // `wlr_xdg_toplevel` always has a non-null `base`; see
        // `configure_toplevel`'s identical argument for that second claim.
        let initialized = unsafe { (*(*entry.raw.as_ptr()).base).initialized };

        if initialized {
            let mode = if server_side {
                sys::wlr_xdg_toplevel_decoration_v1_mode::WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE
            } else {
                sys::wlr_xdg_toplevel_decoration_v1_mode::WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_CLIENT_SIDE
            };
            // SAFETY: a present entry names a decoration that is still
            // linked into `self.inner.decorations` — removed synchronously,
            // before wlroots frees it, by
            // `forget_decoration`/`forget_toplevel` (see
            // `RuntimeInner::decorations`'s own doc) — so `raw` is live,
            // and `initialized` being true means the
            // `assert(surface->initialized)` this method's own doc
            // describes cannot fire.
            unsafe { sys::wlr_xdg_toplevel_decoration_v1_set_mode(raw.as_ptr(), mode) };
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
            entry.staged.set(Some(server_side));
        }

        if let Some(entry) = self.inner.decorations.borrow().get(&id) {
            entry.mode_set_this_dispatch.set(true);
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
    /// the toplevel's scene tree is a direct child of the scene root (see
    /// `backend.rs`'s `wlr_scene_xdg_surface_create` call), so its node's
    /// `x`/`y` *are* its scene-absolute origin, and `(x, y)` minus that is
    /// the window-relative position regardless of how deep the struck leaf
    /// was.
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
                parent: Some(toplevel),
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
            rt.set_decoration_mode(id, true),
            Some(()),
            "staging still reports success to the caller"
        );
        assert!(
            rt.decoration_dispatch_flag(id),
            "the request-answered flag is set even when the send is only staged"
        );
        assert_eq!(
            rt.take_staged_decoration_mode(id),
            Some(true),
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
            .set(Some(true));
        assert_eq!(
            rt.inner.decorations.borrow().get(&id).unwrap().staged.get(),
            Some(true),
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
}
