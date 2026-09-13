//! Borrow-scoped `wlr_surface` handles and their stable ids.
//!
//! Same shape as [`Toplevel`](crate::Toplevel) and [`Output`](crate::Output),
//! for the same reason: a `wlr_surface` is freed whenever its client (or its
//! role object) says so, so a handle that escapes the handler it was passed to
//! is a use-after-free. The lifetime and the private constructor make that a
//! compile error.
//!
//! Unlike a role handle, a [`Surface`] is the *generic* view of a
//! `wlr_surface`: it carries no role state, only the identity every
//! `wlr_surface` has and the committed geometry every one of them exposes.
//! Role handles ([`Toplevel`](crate::Toplevel), [`Popup`](crate::Popup),
//! [`LayerSurface`](crate::LayerSurface)) stay the way to reach role-specific
//! state; this is what a consumer gets when all it has is a surface id.
//!
//! The id is attached to the surface's own addon set under a kind distinct
//! from the role id's ([`crate::id`]'s `SURFACE_ID_ADDON_IMPL` versus
//! `ID_ADDON_IMPL`), so one surface carries both and each resolver finds only
//! its own. wlroots runs the addon's destructor when the surface dies, so the
//! id stops resolving at exactly the right moment and nothing has to be swept.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::sys;

/// Identifies a `wlr_surface` for as long as the consumer chooses to remember
/// it.
///
/// Storable, comparable and hashable — unlike a handle. Ids are never reused
/// within a process, and an id held past its surface's destruction resolves to
/// nothing rather than to another surface.
///
/// Deliberately no `PartialOrd`/`Ord`, for the identical reason
/// [`ToplevelId`](crate::ToplevelId) documents: an opaque id ordering would
/// promise creation-order semantics nobody asked for, and this API is frozen
/// within the wlroots minor, so a derive added here could not be withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceId(pub(crate) u64);

impl SurfaceId {
    /// An id no live surface can have, for testing the "unknown id" path.
    ///
    /// Public for the same reason
    /// [`ToplevelId::dangling_for_test`](crate::ToplevelId::dangling_for_test)
    /// is: "every by-id operation reports a miss rather than dereferencing" is
    /// a promise to consumers, and a promise nobody can write a test for is
    /// not one. Ids come from the process-wide counter that backs every id in
    /// this crate, which starts at 1, only increments and never reuses a
    /// value, so `u64::MAX` cannot be handed to a real surface.
    ///
    /// Not for production code. An id from a real surface is the one
    /// [`Surface::id`] returns, and it stops resolving once the
    /// [`Backend::run_all`](crate::Backend::run_all) call that announced it has
    /// returned — at which point it behaves exactly like this one.
    pub fn dangling_for_test() -> SurfaceId {
        SurfaceId(u64::MAX)
    }

    /// A distinct id no live surface can have, for testing.
    ///
    /// The `SurfaceId` counterpart of
    /// [`ToplevelId::dangling_nth_for_test`](crate::ToplevelId::dangling_nth_for_test);
    /// see that method's own doc for the reserved-band argument, which applies
    /// verbatim here because both id types draw from the same process-wide
    /// counter.
    ///
    /// `n` is folded into a fixed 2^32-wide band immediately below `u64::MAX`
    /// (`n % 2^32`), and `n = 0` aliases
    /// [`dangling_for_test`](Self::dangling_for_test); callers wanting an id
    /// distinct from every other test id must pass `n >= 1`.
    pub fn dangling_nth_for_test(n: u64) -> SurfaceId {
        SurfaceId(u64::MAX - (n % (1u64 << 32)))
    }
}

/// A surface, borrowed for the duration of a handler call.
pub struct Surface<'h> {
    raw: NonNull<sys::wlr_surface>,
    id: SurfaceId,
    /// The runtime's `wp_tearing_control_manager_v1`, cached when the handle is
    /// built so [`Surface::tearing_hint`](crate::Surface::tearing_hint) can read
    /// the surface's hint without a second lookup. `None` when no manager has
    /// been created, or for a handle built outside a runtime (the tests'
    /// scratch constructor) — in which case the tearing accessors miss rather
    /// than return a wrong default.
    tearing_manager: Option<NonNull<sys::wlr_tearing_control_manager_v1>>,
    _scope: PhantomData<&'h ()>,
}

/// Hand-written rather than derived, for the same reason
/// [`Toplevel`](crate::Toplevel)'s is: the `PhantomData` scope marker has no
/// value to print, and a raw pointer printed by a derive is neither useful nor
/// stable across runs.
impl std::fmt::Debug for Surface<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl<'h> Surface<'h> {
    /// # Safety
    ///
    /// `raw` must be a live `wlr_surface` whose addon set carries `id`, and the
    /// returned handle must not outlive the callback it was created for.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_surface,
        id: SurfaceId,
    ) -> Surface<'h> {
        Surface {
            raw: NonNull::new(raw).expect("wlroots handed us a null surface"),
            id,
            tearing_manager: None,
            _scope: PhantomData,
        }
    }

    /// Attach the runtime's tearing-control manager, so the tearing accessors
    /// can reach it.
    ///
    /// Consuming builder rather than a setter because `Surface` is not `mut` at
    /// its construction sites and the field is private to this module; the
    /// only two production builders (`Runtime::surface` and
    /// `backend::with_surface`) call it once, right after `from_raw_with_id`.
    pub(crate) fn with_tearing_manager(
        mut self,
        manager: Option<NonNull<sys::wlr_tearing_control_manager_v1>>,
    ) -> Surface<'h> {
        self.tearing_manager = manager;
        self
    }

    /// The raw surface, for the in-crate callers that pass it to wlroots.
    pub(crate) fn as_ptr(&self) -> *mut sys::wlr_surface {
        self.raw.as_ptr()
    }

    /// The tearing-control manager cached on this handle, if the runtime had
    /// one when the handle was built.
    pub(crate) fn tearing_manager(&self) -> Option<NonNull<sys::wlr_tearing_control_manager_v1>> {
        self.tearing_manager
    }

    /// This surface's stable identity, safe to store beyond the handler.
    pub fn id(&self) -> SurfaceId {
        self.id
    }

    /// The surface's committed size, `(width, height)`, in surface-local
    /// pixels.
    ///
    /// `(0, 0)` before the client's first commit, since that is what wlroots'
    /// own `wlr_surface_state` starts zeroed at and this reads it directly
    /// rather than tracking a separate "has it committed yet" flag of its own.
    pub fn current_size(&self) -> (i32, i32) {
        // SAFETY: the handle borrows a live surface for its lifetime, and
        // `current` is a plain embedded struct (not a pointer) that is always
        // initialised once the surface exists.
        unsafe {
            let current = &(*self.raw.as_ptr()).current;
            (current.width, current.height)
        }
    }

    /// Whether any buffer is attached — the surface is mapped, or ready to be.
    ///
    /// A surface has a buffer when it last committed a non-null one; a
    /// surface that never committed, or committed a null buffer, reports
    /// `false`.
    pub fn has_buffer(&self) -> bool {
        // SAFETY: the handle borrows a live surface for its lifetime.
        unsafe { sys::wlr_surface_has_buffer(self.raw.as_ptr()) }
    }

    /// Whether wlroots currently considers this surface mapped.
    ///
    /// This is wlroots' own `wlr_surface.mapped` flag, not a derived guess:
    /// it goes true when a buffered commit lands and false when the surface
    /// unmaps (a null buffer, or the role object going away).
    pub fn mapped(&self) -> bool {
        // SAFETY: the handle borrows a live surface for its lifetime.
        unsafe { (*self.raw.as_ptr()).mapped }
    }
}

#[cfg(test)]
mod tests {
    use super::{Surface, SurfaceId};
    use crate::sys;
    use std::alloc::{Layout, alloc_zeroed, dealloc};

    /// A zeroed, heap-allocated `wlr_surface`, wired up just enough for
    /// [`Surface::current_size`], [`Surface::has_buffer`] and
    /// [`Surface::mapped`] to read real values.
    ///
    /// `alloc_zeroed` rather than `std::mem::zeroed`, for the same reason
    /// `toplevel.rs`'s `ScratchToplevel` uses it: `wlr_surface` embeds
    /// `wl_signal`/`wl_listener` machinery with bare function pointers, a bit
    /// pattern `std::mem::zeroed` refuses to produce as a materialised *value*.
    /// Touching the bytes only through a raw pointer sidesteps that.
    struct ScratchSurface {
        surface: *mut sys::wlr_surface,
    }

    impl ScratchSurface {
        fn new() -> Self {
            let layout = Layout::new::<sys::wlr_surface>();
            // SAFETY: `wlr_surface` is a nonzero-sized type, so `alloc_zeroed`
            // returns either null (checked below) or a suitably aligned,
            // zeroed allocation of exactly that size.
            let surface = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_surface>();
            assert!(!surface.is_null(), "allocation failed");
            Self { surface }
        }

        /// # Safety
        ///
        /// The returned handle borrows this `ScratchSurface`'s allocation and
        /// must not outlive it.
        unsafe fn surface(&self, id: SurfaceId) -> Surface<'_> {
            // SAFETY: `self.surface` is a live allocation for as long as
            // `self` is; the caller upholds the lifetime bound.
            unsafe { Surface::from_raw_with_id(self.surface, id) }
        }
    }

    impl Drop for ScratchSurface {
        fn drop(&mut self) {
            // SAFETY: `self.surface` was allocated by `alloc_zeroed` with the
            // matching layout, is still exclusively owned, and nothing else
            // frees or aliases it.
            unsafe { dealloc(self.surface.cast(), Layout::new::<sys::wlr_surface>()) };
        }
    }

    #[test]
    fn current_size_reads_the_committed_state() {
        let scratch = ScratchSurface::new();
        // SAFETY: `scratch` outlives every use of the handle below.
        unsafe {
            (*scratch.surface).current.width = 640;
            (*scratch.surface).current.height = 480;
        }
        let surface = unsafe { scratch.surface(SurfaceId(0)) };
        assert_eq!(surface.current_size(), (640, 480));
        assert_eq!(surface.id(), SurfaceId(0));
    }

    #[test]
    fn current_size_is_zero_before_any_commit() {
        let scratch = ScratchSurface::new();
        let surface = unsafe { scratch.surface(SurfaceId(0)) };
        assert_eq!(surface.current_size(), (0, 0));
    }

    #[test]
    fn has_buffer_and_mapped_reflect_the_surface_fields() {
        let scratch = ScratchSurface::new();
        // Copied before the handle borrows `scratch`, so the writes below go
        // through an independent raw pointer and do not conflict with the
        // handle's shared borrow.
        let p = scratch.surface;
        let surface = unsafe { scratch.surface(SurfaceId(0)) };
        assert!(!surface.has_buffer(), "a fresh surface has no buffer");
        assert!(!surface.mapped(), "and is not mapped");

        // SAFETY: `scratch` outlives the handle. `wlr_surface_has_buffer`
        // reports the committed buffer dimensions (`buffer_width`/
        // `buffer_height`), so setting those is the minimal real witness for
        // its true path; `mapped` is a plain bool field.
        unsafe {
            (*p).current.buffer_width = 16;
            (*p).current.buffer_height = 16;
            (*p).mapped = true;
        }
        assert!(surface.has_buffer(), "a buffered surface reports true");
        assert!(surface.mapped(), "the mapped flag is read through");
    }

    /// Both test constructors sit at the top of the id space, which a
    /// process-wide counter starting at 1 and only ever incrementing can never
    /// reach — so neither can collide with a real surface's id.
    #[test]
    fn test_constructors_never_collide_with_a_real_id() {
        assert!(SurfaceId::dangling_for_test().0 > u32::MAX as u64);
        for n in 1..=8 {
            assert!(SurfaceId::dangling_nth_for_test(n).0 > u32::MAX as u64);
        }
        assert_eq!(
            SurfaceId::dangling_nth_for_test(0),
            SurfaceId::dangling_for_test()
        );
        for n in 1..=8 {
            assert_ne!(
                SurfaceId::dangling_for_test(),
                SurfaceId::dangling_nth_for_test(n)
            );
        }
    }
}
