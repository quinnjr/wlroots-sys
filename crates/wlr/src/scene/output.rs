//! Scene outputs: the viewport that turns a scene into pixels on one output,
//! and the timer that says how long that took.
//!
//! # What a scene output is
//!
//! One per `wlr_output` per scene, created by
//! [`Runtime::init_output`](crate::Runtime::init_output) (or by
//! [`Runtime::add_scene_output`](crate::Runtime::add_scene_output) for an
//! output kept out of the layout), and owned by the scene. It carries the
//! output's position in layout coordinates and its own
//! [damage ring](crate::DamageRingRef), and
//! [`Runtime::commit_scene_output`](crate::Runtime::commit_scene_output) is
//! what renders and presents through it.
//!
//! # Why the id needs a listener rather than an addon
//!
//! Every other id in this crate hangs off a `wlr_addon_set` — the object's own
//! destructor releases it, and a stale id misses for free (see
//! [`NodeId`](crate::NodeId)). A `wlr_scene_output` has no addon *set*: its
//! single `addon` field is one wlroots itself plants on the `wlr_output`, and
//! there is nowhere for a second one to go. So [`SceneOutputId`] is backed by a
//! listener on `wlr_scene_output.events.destroy` instead, which fires on both
//! paths that free one — an explicit
//! [`destroy_scene_output`](crate::Runtime::destroy_scene_output), and the
//! output itself dying (an unplug, or the display being torn down, both of
//! which reach `wlr_scene_output_destroy` through wlroots' own
//! `output_destroy` listener).
//!
//! The listener is owned by the [`Runtime`](crate::Runtime), not by a
//! [`Backend::run_all`](crate::Backend::run_all) call, which is the one place
//! this crate's per-run listener discipline does not apply: a scene output
//! outlives the run that created it (the scene does), so a run-scoped watch
//! would leave the table lying the moment the run returned.

use std::cell::Cell;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};

use crate::render::{ColorTransform, Swapchain};
use crate::runtime::RuntimeInner;
use crate::scene::DamageRingRef;
use crate::sys;

/// Identifies a scene output for as long as the consumer chooses to remember
/// it.
///
/// Storable, comparable and hashable — unlike [`SceneOutput`], which cannot
/// escape the borrow that produced it. One goes stale the instant its scene
/// output is destroyed, whether that was
/// [`Runtime::destroy_scene_output`](crate::Runtime::destroy_scene_output) or
/// the `wlr_output` underneath it dying; every by-id call reports `None` from
/// then on.
///
/// Unlike an [`OutputId`](crate::OutputId), this is **not** scoped to one
/// [`Backend::run_all`](crate::Backend::run_all) call — a scene output belongs
/// to the scene, which outlives any run, and the destroy listener that keeps
/// this table truthful belongs to the [`Runtime`](crate::Runtime) rather than
/// to a run.
///
/// Deliberately no `PartialOrd`/`Ord`, for the reason
/// [`OutputId`](crate::OutputId) records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SceneOutputId(pub(crate) u64);

impl SceneOutputId {
    /// An id no live scene output can have, for testing that an unknown id
    /// misses cleanly rather than crashing.
    ///
    /// The same reserved top of the counter's range
    /// [`ToplevelId::dangling_for_test`](crate::ToplevelId::dangling_for_test)
    /// uses, and for the same reason.
    pub fn dangling_for_test() -> SceneOutputId {
        SceneOutputId(u64::MAX)
    }
}

/// The destroy listener that keeps `RuntimeInner::scene_outputs` truthful.
///
/// `#[repr(C)]` with `listener` first, because wlroots hands the callback a
/// `*mut wl_listener` pointing at that field and the callback casts it back —
/// the same `container_of` shape `backend.rs`'s `Bound` uses, pinned the same
/// way.
#[repr(C)]
pub(crate) struct SceneOutputWatch {
    listener: sys::wl_listener,
    /// Weak, never strong: a strong reference would be a cycle through the very
    /// table this removes a row from.
    runtime: Weak<RuntimeInner>,
    id: SceneOutputId,
    /// Shared with the table row. Cleared before the row is removed, so a
    /// lookup misses even if the removal itself could not take the borrow.
    alive: Rc<Cell<bool>>,
    /// Whether [`Drop`] still has an unlink to do.
    ///
    /// Cleared by the callback, which unlinks *itself* while the scene output
    /// is still alive and then drops the box by removing the row. Without this
    /// the box's own `Drop` would unlink a second time, from a list wlroots has
    /// since freed.
    linked: Cell<bool>,
}

const _: () = assert!(std::mem::offset_of!(SceneOutputWatch, listener) == 0);

impl Drop for SceneOutputWatch {
    fn drop(&mut self) {
        if !self.linked.get() {
            return;
        }
        // SAFETY: `linked` is true, so this listener is still in the scene
        // output's destroy signal, and that signal is only ever freed after
        // emitting — which is the path that clears the flag. So the list and
        // its other members are live here.
        unsafe { crate::backend::remove_listener(&raw mut self.listener) };
    }
}

/// wlroots is about to free this scene output.
///
/// Runs inside `wlr_scene_output_destroy`'s own emission, before the memory
/// goes, which is the only moment at which this crate can learn the id is
/// stale.
unsafe extern "C" fn on_scene_output_destroy(
    listener: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for a listener linked by `watch` below,
    // whose address is the `listener` field of a live, boxed `SceneOutputWatch`
    // (`#[repr(C)]`, field 0, asserted above). Nothing here can unwind: every
    // call below is total.
    unsafe {
        let watch = listener.cast::<SceneOutputWatch>();
        let id = (*watch).id;
        (*watch).alive.set(false);
        let runtime = (*watch).runtime.upgrade();

        // Unlink first, and record that it happened, because removing the row
        // below frees this very box — after which `watch` must not be touched
        // again. Self-unlinking from inside the emission is sound:
        // `wl_signal_emit_mutable` advances its cursor past the firing listener
        // before calling it.
        crate::backend::remove_listener(&raw mut (*watch).listener);
        (*watch).linked.set(false);

        if let Some(inner) = runtime {
            // `try_borrow_mut`, not `borrow_mut`: this frame is `extern "C"`,
            // where a panic is an abort. The crate holds no borrow of this
            // table across a call into wlroots, so it cannot fail — and if it
            // ever did, the flag cleared above has already made every lookup
            // miss, so the worst outcome is a leaked row.
            if let Ok(mut table) = inner.scene_outputs.try_borrow_mut() {
                table.remove(&id);
            }
        }
    }
}

/// A scene output this runtime has an id for.
///
/// Not `Copy`: `alive` is an [`Rc`] shared with the watch, and the watch is a
/// box whose address the listener list holds. Field order is load-bearing in
/// the same way `backend.rs`'s `InputDevice` is — the watch must unlink before
/// the flag it names goes away.
pub(crate) struct SceneOutputEntry {
    pub(crate) raw: NonNull<sys::wlr_scene_output>,
    _watch: Box<SceneOutputWatch>,
    pub(crate) alive: Rc<Cell<bool>>,
}

/// Start watching `raw` for destruction, and build the row that names it.
///
/// # Safety
///
/// `raw` must point at a live `wlr_scene_output` that this runtime does not
/// already watch — a second listener would purge the row twice, the second time
/// through a box that had already been freed.
pub(crate) unsafe fn watch(
    raw: NonNull<sys::wlr_scene_output>,
    runtime: &Rc<RuntimeInner>,
    id: SceneOutputId,
) -> SceneOutputEntry {
    let alive = Rc::new(Cell::new(true));
    let mut boxed = Box::new(SceneOutputWatch {
        listener: sys::wl_listener {
            // `wl_signal_add` overwrites both ends via `wl_list_insert`, so
            // these nulls are never read.
            link: sys::wl_list {
                prev: std::ptr::null_mut(),
                next: std::ptr::null_mut(),
            },
            notify: on_scene_output_destroy,
        },
        runtime: Rc::downgrade(runtime),
        id,
        alive: Rc::clone(&alive),
        linked: Cell::new(true),
    });
    // SAFETY: the caller guarantees `raw` is live, so `events.destroy` is an
    // initialised signal; the listener is freshly boxed and linked nowhere
    // else, and its address stays put until the entry drops — which unlinks it
    // first.
    unsafe {
        sys::wl_signal_add(
            &raw mut (*raw.as_ptr()).events.destroy,
            &raw mut boxed.listener,
        )
    };
    SceneOutputEntry {
        raw,
        _watch: boxed,
        alive,
    }
}

/// A borrow-scoped view of one scene output.
///
/// Read-only, and cannot outlive the borrow that produced it — the usual
/// bargain in this crate. Mutation goes through [`Runtime`](crate::Runtime) by
/// [`SceneOutputId`], which misses cleanly on a scene output that has since
/// been destroyed; a handle cannot, which is why it is scoped.
pub struct SceneOutput<'h> {
    raw: NonNull<sys::wlr_scene_output>,
    id: SceneOutputId,
    _scope: PhantomData<&'h ()>,
}

impl<'h> SceneOutput<'h> {
    /// # Safety
    ///
    /// * `raw` must point at a live `wlr_scene_output`.
    /// * `id` must be the id this runtime recorded for it.
    /// * The handle must not outlive the borrow that established both.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_scene_output,
        id: SceneOutputId,
    ) -> SceneOutput<'h> {
        SceneOutput {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_scene_output"),
            id,
            _scope: PhantomData,
        }
    }

    /// This scene output's id — the storable half of the pair.
    pub fn id(&self) -> SceneOutputId {
        self.id
    }

    /// This viewport's top-left corner in layout coordinates.
    ///
    /// Kept in step with the output layout automatically for an output
    /// [`Runtime::init_output`](crate::Runtime::init_output) placed, because
    /// `wlr_scene_output_layout_add_output` links the two; set by hand with
    /// [`Runtime::set_scene_output_position`](crate::Runtime::set_scene_output_position)
    /// otherwise.
    pub fn position(&self) -> (i32, i32) {
        // SAFETY: the handle's lifetime guarantees the scene output is live.
        unsafe { ((*self.raw.as_ptr()).x, (*self.raw.as_ptr()).y) }
    }

    /// Whether the scene has something new to draw on this output.
    ///
    /// `false` means a commit can be skipped entirely for this frame — which is
    /// exactly the test `wlr_scene_output_commit` makes for itself before doing
    /// any work (verified by disassembling `libwlroots-0.20.so`: the commit's
    /// first act is this call, returning `true` immediately when it is false).
    pub fn needs_frame(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the scene output is live.
        unsafe { sys::wlr_scene_output_needs_frame(self.raw.as_ptr()) }
    }

    /// This scene output's damage ring, in **buffer-local** coordinates.
    ///
    /// Borrowed, and deliberately so: the ring is embedded in the scene output
    /// by value, initialised and finished by the scene, so nothing outside
    /// wlroots may `init` or `finish` it — which is why this is a
    /// [`DamageRingRef`] and not a [`DamageRing`](crate::DamageRing). It cannot
    /// outlive this handle.
    pub fn damage_ring(&self) -> DamageRingRef<'_> {
        // SAFETY: the handle's lifetime guarantees the scene output is live,
        // and its `damage_ring` is embedded by value and initialised by
        // `wlr_scene_output_create` for the scene output's whole life.
        unsafe { DamageRingRef::from_raw(&raw mut (*self.raw.as_ptr()).damage_ring) }
    }

    /// The raw scene output, for a consumer reaching past this crate. Valid
    /// only for as long as the handle is.
    pub fn as_ptr(&self) -> *mut sys::wlr_scene_output {
        self.raw.as_ptr()
    }
}

impl std::fmt::Debug for SceneOutput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneOutput")
            .field("id", &self.id)
            .field("position", &self.position())
            .field("needs_frame", &self.needs_frame())
            .finish()
    }
}

/// What a scene-output commit should do beyond rendering and presenting.
///
/// Every field is optional and the default is what wlroots reads out of a
/// zeroed `wlr_scene_output_state_options`: no timing, the output's own colour
/// pipeline, and the scene's own swapchain.
#[derive(Debug, Default)]
pub struct SceneOutputStateOptions<'a> {
    timer: Option<&'a SceneTimer>,
    color_transform: Option<&'a ColorTransform>,
    swapchain: Option<&'a Swapchain<'a>>,
}

impl<'a> SceneOutputStateOptions<'a> {
    /// Options that time nothing, transform nothing and swap through the
    /// scene's own swapchain.
    pub fn new() -> SceneOutputStateOptions<'a> {
        SceneOutputStateOptions::default()
    }

    /// Measure this commit with `timer`.
    ///
    /// The timer is reusable: `wlr_scene_output_build_state`'s first act is to
    /// finish and zero whatever is in it, so a timer handed to two commits
    /// reports the second and leaks nothing from the first (verified by
    /// disassembling `libwlroots-0.20.so`).
    pub fn timer(mut self, timer: &'a SceneTimer) -> Self {
        self.timer = Some(timer);
        self
    }

    /// Apply `transform` before the output's own colour transform.
    ///
    /// wlroots **cannot** combine this with an image description set on the
    /// output; the commit fails rather than choosing one.
    pub fn color_transform(mut self, transform: &'a ColorTransform) -> Self {
        self.color_transform = Some(transform);
        self
    }

    /// Render into `swapchain` instead of the output's own.
    ///
    /// For trying an output configuration out before committing to it. The
    /// swapchain's dimensions must match the output's, or the commit fails.
    pub fn swapchain(mut self, swapchain: &'a Swapchain<'a>) -> Self {
        self.swapchain = Some(swapchain);
        self
    }

    /// The C struct these options describe, borrowing `self` for the call.
    ///
    /// The pointers it holds are borrowed from `'a`, so the value must not
    /// outlive `self`. Built at the call site rather than stored.
    pub(crate) fn as_c(&self) -> sys::wlr_scene_output_state_options {
        sys::wlr_scene_output_state_options {
            timer: self.timer.map_or(std::ptr::null_mut(), SceneTimer::as_ptr),
            color_transform: self
                .color_transform
                .map_or(std::ptr::null_mut(), ColorTransform::as_ptr),
            swapchain: self
                .swapchain
                .map_or(std::ptr::null_mut(), Swapchain::as_ptr),
        }
    }
}

/// How long a scene-output commit took, including the work that finished on
/// the GPU after the call returned.
///
/// Caller-allocated wlroots memory — `wlr_scene_timer` is a value a compositor
/// keeps, not an object wlroots hands out — so this owns it and its [`Drop`]
/// calls `wlr_scene_timer_finish`, releasing the render timer inside. One of
/// only two places in this crate where a `Drop` on a wlroots type is right
/// (the other is [`DamageRing`](crate::DamageRing)).
///
/// Hand one to a commit through
/// [`SceneOutputStateOptions::timer`], then read
/// [`duration_ns`](Self::duration_ns) once that commit has returned.
pub struct SceneTimer {
    /// Boxed for a stable address — wlroots writes into the struct through the
    /// pointer the options carry, and that write happens after the value was
    /// handed over. [`UnsafeCell`](std::cell::UnsafeCell) because every method
    /// here takes `&self` while wlroots mutates through that same pointer;
    /// deriving a `*mut` from a shared reference to write through would be
    /// undefined behaviour without it.
    inner: Box<std::cell::UnsafeCell<sys::wlr_scene_timer>>,
}

impl SceneTimer {
    /// A timer that has measured nothing yet.
    pub fn new() -> SceneTimer {
        // Zeroed is the initialised state: `wlr_scene_timer` is an `int64_t`
        // and a nullable pointer, wlroots ships no `wlr_scene_timer_init`, and
        // `wlr_scene_output_build_state` zeroes the struct itself before using
        // it.
        //
        // SAFETY: all-zero is a valid bit pattern for both fields.
        SceneTimer {
            inner: Box::new(std::cell::UnsafeCell::new(unsafe {
                std::mem::zeroed::<sys::wlr_scene_timer>()
            })),
        }
    }

    /// The raw timer. Valid for as long as this value is, and stable across
    /// moves of it.
    pub fn as_ptr(&self) -> *mut sys::wlr_scene_timer {
        self.inner.get()
    }

    /// How long the last commit took, in nanoseconds.
    ///
    /// The pre-render duration plus what the renderer's own timer measured.
    /// `None` when that renderer timer has no answer yet — the GPU work has not
    /// finished, or the renderer does not support timing at all — which wlroots
    /// spells as `-1` and this does not flatten to `0`, since that would report
    /// a measurement that never happened.
    ///
    /// A timer that has **not** been through a commit reports `Some(0)`, not
    /// `None`: it has no renderer timer to ask, so wlroots returns the
    /// pre-render duration alone, which is zero (verified by disassembling
    /// `libwlroots-0.20.so`). Ask
    /// [`pre_render_duration_ns`](Self::pre_render_duration_ns) if the two
    /// halves need telling apart.
    pub fn duration_ns(&self) -> Option<i64> {
        // SAFETY: this value owns a live, initialised timer for its whole life;
        // the call reads it and the render timer it names.
        let ns = unsafe { sys::wlr_scene_timer_get_duration_ns(self.as_ptr()) };
        (ns >= 0).then_some(ns)
    }

    /// How long the commit spent *before* handing work to the GPU, in
    /// nanoseconds.
    ///
    /// A plain field read, and `0` until a commit has written it — unlike
    /// [`duration_ns`](Self::duration_ns), wlroots has no "unavailable"
    /// spelling for this one.
    pub fn pre_render_duration_ns(&self) -> i64 {
        // SAFETY: as above; this reads one `int64_t` field.
        unsafe { (*self.as_ptr()).pre_render_duration }
    }
}

impl Default for SceneTimer {
    fn default() -> SceneTimer {
        SceneTimer::new()
    }
}

impl std::fmt::Debug for SceneTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneTimer")
            .field("duration_ns", &self.duration_ns())
            .field("pre_render_duration_ns", &self.pre_render_duration_ns())
            .finish()
    }
}

impl Drop for SceneTimer {
    fn drop(&mut self) {
        // SAFETY: this value owns the timer and nothing else can have finished
        // it — `wlr_scene_timer_finish` is reachable from nowhere else in this
        // crate. wlroots' own call to it inside `wlr_scene_output_build_state`
        // zeroes the struct afterwards, so a timer that has been through a
        // commit has nothing left to release and this is a null check.
        unsafe { sys::wlr_scene_timer_finish(self.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions::assert_not_impl_any;

    /// The `IN_HANDLER` argument in `dispatch.rs` rests on every handle being
    /// thread-bound, and both owned types here hold C allocations.
    #[test]
    fn scene_output_types_are_thread_bound() {
        assert_not_impl_any!(SceneOutput<'static>: Send, Sync);
        assert_not_impl_any!(SceneTimer: Send, Sync);
        assert_not_impl_any!(SceneOutputStateOptions<'static>: Send, Sync);
    }

    /// A fresh timer reports a zero duration rather than `None`, which is what
    /// wlroots actually does with no renderer timer to ask — pinned here
    /// because the header's "returns -1 if the duration is unavailable" reads
    /// like it covers this case and does not.
    #[test]
    fn a_fresh_timer_reports_zero_rather_than_unavailable() {
        let timer = SceneTimer::new();
        assert_eq!(timer.duration_ns(), Some(0));
        assert_eq!(timer.pre_render_duration_ns(), 0);
    }

    /// Default options are the zeroed C struct, which is what "no timer, no
    /// transform, the scene's own swapchain" is spelled as.
    #[test]
    fn default_options_are_three_null_pointers() {
        let options = SceneOutputStateOptions::new();
        let c = options.as_c();
        assert!(c.timer.is_null());
        assert!(c.color_transform.is_null());
        assert!(c.swapchain.is_null());
    }

    /// A timer plugged into the options is the very timer the commit will
    /// write through — a copy would silently measure nothing.
    #[test]
    fn a_plugged_in_timer_is_passed_by_address() {
        let timer = SceneTimer::new();
        let options = SceneOutputStateOptions::new().timer(&timer);
        assert_eq!(options.as_c().timer, timer.as_ptr());
    }

    /// Dangling ids must sit in the reserved top of the counter's range.
    #[test]
    fn a_dangling_scene_output_id_cannot_collide_with_an_issued_one() {
        assert_eq!(SceneOutputId::dangling_for_test(), SceneOutputId(u64::MAX));
        assert!(crate::id::next_id() < u64::MAX / 2);
    }
}
