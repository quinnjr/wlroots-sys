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

use crate::id::{SourceId, next_id};
use crate::scene::RectId;
use crate::{Backend, Display, Error, Interest, Output, Result, ToplevelId, sys};

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

    pub(crate) rects: RefCell<HashMap<RectId, NonNull<sys::wlr_scene_rect>>>,

    /// The xdg shell, once created. `Option` because a consumer that only
    /// wants a scene never makes one, and because a second one would
    /// advertise a second `xdg_wm_base` global.
    pub(crate) xdg_shell: RefCell<Option<NonNull<sys::wlr_xdg_shell>>>,

    /// Every live toplevel: the role object, its scene tree, and the surface
    /// its id addon lives on.
    pub(crate) toplevels: RefCell<HashMap<ToplevelId, ToplevelEntry>>,

    /// Reverse lookup for the scene hit test, which finds a `wlr_scene_tree`
    /// and has to name the toplevel it belongs to. Keyed by the tree pointer
    /// because that is what `wlr_scene_node_at` walks back to.
    pub(crate) tree_to_toplevel: RefCell<HashMap<usize, ToplevelId>>,
}

#[derive(Clone, Copy)]
pub(crate) struct ToplevelEntry {
    pub(crate) raw: NonNull<sys::wlr_xdg_toplevel>,
    pub(crate) tree: NonNull<sys::wlr_scene_tree>,
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
                xdg_shell: RefCell::new(None),
                toplevels: RefCell::new(HashMap::new()),
                tree_to_toplevel: RefCell::new(HashMap::new()),
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
    /// next one. There is no removal by id in 0.20.1; a source lives as long
    /// as the runtime.
    pub fn add_fd(&self, fd: OwnedFd, interest: Interest) -> SourceId {
        let id = SourceId(next_id());
        self.inner.sources.borrow_mut().push(FdSource { fd, interest, id });
        id
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
            sources.iter().find(|s| s.id == id).map(|s| s.fd.as_raw_fd())
        }?;
        // SAFETY: `raw` came from an `OwnedFd` this runtime owns, and this
        // handle keeps that `OwnedFd` alive for the whole call — nothing
        // removes a source in 0.20.1, and `f` cannot reach one if it did.
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

            Graphics { scene, layout, scene_layout, renderer, allocator }
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
                None => return Err(Error::Operation("Runtime::init_output before init_graphics")),
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
                None => return Err(Error::Operation("Runtime::commit_output before init_graphics")),
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
        self.inner.rects.borrow_mut().insert(id, raw);
        Ok(id)
    }

    /// Move a rect. `None` if this runtime never issued `rect`.
    pub fn set_rect_position(&self, rect: RectId, x: i32, y: i32) -> Option<()> {
        let raw = self.rect_ptr(rect)?;
        // SAFETY: rects live as long as this runtime (nothing removes one),
        // and the table only ever holds pointers `add_rect` created.
        unsafe { sys::wlr_scene_node_set_position(&raw mut (*raw.as_ptr()).node, x, y) };
        Some(())
    }

    /// Resize a rect. `None` if this runtime never issued `rect`.
    pub fn set_rect_size(&self, rect: RectId, width: i32, height: i32) -> Option<()> {
        let raw = self.rect_ptr(rect)?;
        // SAFETY: as for `set_rect_position`.
        unsafe { sys::wlr_scene_rect_set_size(raw.as_ptr(), width, height) };
        Some(())
    }

    /// Recolour a rect, in the same premultiplied RGBA
    /// [`add_rect`](Runtime::add_rect) takes. `None` if this runtime never
    /// issued `rect`.
    pub fn set_rect_color(&self, rect: RectId, color: [f32; 4]) -> Option<()> {
        let raw = self.rect_ptr(rect)?;
        // SAFETY: as above; `color` is live for the call and wlroots copies it.
        unsafe { sys::wlr_scene_rect_set_color(raw.as_ptr(), color.as_ptr()) };
        Some(())
    }

    /// Put a rect behind everything else in the scene. `None` if this runtime
    /// never issued `rect`.
    pub fn lower_rect_to_bottom(&self, rect: RectId) -> Option<()> {
        let raw = self.rect_ptr(rect)?;
        // SAFETY: as above.
        unsafe { sys::wlr_scene_node_lower_to_bottom(&raw mut (*raw.as_ptr()).node) };
        Some(())
    }

    /// The rect `id` names, with the table borrow released before returning —
    /// every caller then re-enters wlroots, which can emit a signal, which can
    /// take this same `RefCell` mutably.
    fn rect_ptr(&self, id: RectId) -> Option<NonNull<sys::wlr_scene_rect>> {
        self.inner.rects.borrow().get(&id).copied()
    }

    /// Advertise `xdg_wm_base` at `version`.
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
    /// [`Error::Operation`] if a shell already exists on this runtime;
    /// [`Error::Create`] if wlroots could not create it.
    pub fn create_xdg_shell(&self, display: &Display, version: u32) -> Result<()> {
        if self.inner.xdg_shell.borrow().is_some() {
            return Err(Error::Operation("Runtime::create_xdg_shell called twice"));
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
        self.inner.toplevels.borrow_mut().insert(id, ToplevelEntry { raw, tree });
        self.inner.tree_to_toplevel.borrow_mut().insert(tree.as_ptr() as usize, id);
    }

    /// Remove `id` from both tables. Called from `on_toplevel_destroy` before
    /// the toplevel is freed.
    pub(crate) fn forget_toplevel(&self, id: ToplevelId) {
        let entry = self.inner.toplevels.borrow_mut().remove(&id);
        if let Some(entry) = entry {
            self.inner.tree_to_toplevel.borrow_mut().remove(&(entry.tree.as_ptr() as usize));
        }
    }

    /// The entry `id` names, with the borrow released before returning — the
    /// caller then re-enters wlroots, which can emit a signal, which can take
    /// this same `RefCell` mutably.
    pub(crate) fn toplevel_entry(&self, id: ToplevelId) -> Option<ToplevelEntry> {
        self.inner.toplevels.borrow().get(&id).copied()
    }

    /// Stage a size on the toplevel's next configure, in **content**
    /// (client-owned) pixels.
    ///
    /// Staged, not sent: wlroots coalesces every state change made in one
    /// event-loop turn into a single configure, so setting a size, an
    /// activation and a maximized flag in the same handler produces one
    /// configure carrying all three rather than three configures.
    ///
    /// `None` if this runtime has no live toplevel with that id.
    pub fn set_toplevel_size(&self, id: ToplevelId, width: i32, height: i32) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: an entry is removed by the destroy callback, which wlroots
        // runs before it frees the toplevel, so a present entry names a live
        // one. `wlr_xdg_toplevel_set_size` only writes pending state.
        unsafe { sys::wlr_xdg_toplevel_set_size(entry.raw.as_ptr(), width, height) };
        Some(())
    }

    /// Stage the `activated` state — the one a client renders its own title
    /// bar and focus ring from. `None` for an unknown id.
    pub fn set_toplevel_activated(&self, id: ToplevelId, activated: bool) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as for `set_toplevel_size`.
        unsafe { sys::wlr_xdg_toplevel_set_activated(entry.raw.as_ptr(), activated) };
        Some(())
    }

    /// Stage the `maximized` state. `None` for an unknown id.
    pub fn set_toplevel_maximized(&self, id: ToplevelId, maximized: bool) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as above.
        unsafe { sys::wlr_xdg_toplevel_set_maximized(entry.raw.as_ptr(), maximized) };
        Some(())
    }

    /// Stage the `fullscreen` state. `None` for an unknown id.
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
    /// `None` for an unknown id.
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
    /// `None` for an unknown id.
    pub fn set_toplevel_visible(&self, id: ToplevelId, visible: bool) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as for `set_toplevel_position`.
        unsafe { sys::wlr_scene_node_set_enabled(&raw mut (*entry.tree.as_ptr()).node, visible) };
        Some(())
    }

    /// Raise the toplevel above every sibling in the scene. `None` for an
    /// unknown id.
    pub fn raise_toplevel(&self, id: ToplevelId) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as above.
        unsafe { sys::wlr_scene_node_raise_to_top(&raw mut (*entry.tree.as_ptr()).node) };
        Some(())
    }

    /// Ask the client to close.
    ///
    /// A request, not a destruction: a well-behaved client may prompt the
    /// user and decline. The toplevel goes away — and
    /// [`ToplevelHandler::toplevel_destroyed`](crate::ToplevelHandler::toplevel_destroyed)
    /// fires — only if and when the client actually destroys it.
    ///
    /// `None` for an unknown id.
    pub fn close_toplevel(&self, id: ToplevelId) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as for `set_toplevel_size`; this only sends a protocol
        // event and cannot free the toplevel synchronously.
        unsafe { sys::wlr_xdg_toplevel_send_close(entry.raw.as_ptr()) };
        Some(())
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
}
