//! Sub-surface roles and the `wlr_subcompositor` global.
//!
//! A `wl_subsurface` gives a client a child surface whose contents and
//! placement are committed by its parent: the sub-surface's position relative
//! to the parent (`wlr_subsurface_parent_state`) is applied when the *parent*
//! commits, not on the child's own commit. A compositor that wants to inspect
//! that relationship holds the parent's [`Surface`] and asks whether it is
//! really a sub-surface.
//!
//! That downcast is the whole reason this module exists. A `wlr_subsurface` is
//! not a free-standing object — it is a *role* laid over an existing
//! `wlr_surface`, created by the client and destroyed whenever the client or
//! the child surface says so. Reachable only through the child surface, it gets
//! no id of its own: the child's [`SurfaceId`] is its identity, and a stale
//! [`Subsurface`] cannot outlive the borrow-scoped [`Surface`] it came from.
//! [`Surface::as_subsurface`] is the entry point, mirroring the crate's other
//! downcasts.
//!
//! The `wlr_subcompositor` global itself is display-owned and created once by
//! [`Runtime::init_graphics`](crate::Runtime::init_graphics); the runtime keeps
//! the pointer it used to discard so a later accessor need not search for it.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::surface::{Surface, SurfaceId};
use crate::sys;

/// The committed placement of a sub-surface relative to its parent.
///
/// An owned snapshot of wlroots' `wlr_subsurface_parent_state`: the fields are
/// copied out when [`Subsurface::parent_state`] runs, so the value is safe to
/// keep for as long as the caller likes. The sub-surface state is not applied
/// on the child's own commit — wlroots applies the parent's `pending` x/y when
/// the **parent** commits — so this reports the last applied placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubsurfaceParentState {
    x: i32,
    y: i32,
}

impl SubsurfaceParentState {
    /// The committed x offset relative to the parent, in surface-local pixels.
    pub fn x(&self) -> i32 {
        self.x
    }

    /// The committed y offset relative to the parent, in surface-local pixels.
    pub fn y(&self) -> i32 {
        self.y
    }
}

/// A sub-surface role, borrowed for the duration of its child surface handle.
///
/// wlroots frees the role when the client destroys the protocol object, the
/// child surface dies, or the display goes away — none of which this handle
/// controls — so it is borrow-scoped like every other handle in this crate
/// rather than owned. Built only by [`Surface::as_subsurface`]; the child's
/// [`SurfaceId`] is the storable identity.
pub struct Subsurface<'h> {
    raw: NonNull<sys::wlr_subsurface>,
    surface: SurfaceId,
    _scope: PhantomData<&'h ()>,
}

/// Hand-written rather than derived, for the same reason [`Surface`]'s is: the
/// `PhantomData` scope marker has no value to print, and the raw pointer a
/// derive would print is neither useful nor stable.
impl std::fmt::Debug for Subsurface<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subsurface")
            .field("surface_id", &self.surface)
            .field("parent_surface_id", &self.parent_surface_id())
            .field("parent_state", &self.parent_state())
            .finish_non_exhaustive()
    }
}

impl<'h> Subsurface<'h> {
    /// Wrap a live `wlr_subsurface` whose child surface is `surface`.
    ///
    /// `assert`/`unwrap`-free by construction: the only producer,
    /// [`Surface::as_subsurface`], has already checked the downcast's non-null
    /// result, so this takes the `NonNull` rather than re-checking a raw one —
    /// the same discipline [`TearingControl`](crate::TearingControl) documents.
    ///
    /// # Safety
    ///
    /// `raw` must be a live `wlr_subsurface` belonging to `surface`, and the
    /// returned handle must not outlive the callback it was created for.
    pub(crate) unsafe fn from_raw_with_id(
        raw: NonNull<sys::wlr_subsurface>,
        surface: SurfaceId,
    ) -> Subsurface<'h> {
        Subsurface {
            raw,
            surface,
            _scope: PhantomData,
        }
    }

    /// The id of the child surface this role belongs to.
    pub fn surface_id(&self) -> SurfaceId {
        self.surface
    }

    /// The id of the parent surface, when this crate tracks it.
    ///
    /// `None` when the parent was never registered — the same miss every
    /// [`SurfaceId`] lookup reports — or when wlroots has already cleared the
    /// link as the parent is torn down.
    pub fn parent_surface_id(&self) -> Option<SurfaceId> {
        // SAFETY: the handle borrows a live sub-surface, whose `parent` field
        // points at the live parent surface (or is null once it is going away).
        let parent = unsafe { (*self.raw.as_ptr()).parent };
        if parent.is_null() {
            return None;
        }
        // SAFETY: `parent` is live, and `find_surface_id` only reads its addon
        // set.
        unsafe { crate::id::find_surface_id(&raw const (*parent).addons) }.map(SurfaceId)
    }

    /// The sub-surface's committed position relative to its parent.
    ///
    /// Read straight from wlroots' `wlr_subsurface_parent_state`, so it is the
    /// placement the parent last applied, not anything this crate tracks.
    pub fn parent_state(&self) -> SubsurfaceParentState {
        // SAFETY: the handle borrows a live sub-surface, and `current` is a
        // plain embedded struct (not a pointer) that is always initialised once
        // the role exists.
        let state: &sys::wlr_subsurface_parent_state = unsafe { &(*self.raw.as_ptr()).current };
        SubsurfaceParentState {
            x: state.x,
            y: state.y,
        }
    }
}

impl<'h> Surface<'h> {
    /// This surface's sub-surface role, if it has one.
    ///
    /// `None` for any surface that is not (or is no longer) a sub-surface —
    /// a plain surface, a toplevel, a popup, or a child whose client destroyed
    /// the role — matching wlroots'
    /// `wlr_subsurface_try_from_wlr_surface`, which returns null in exactly
    /// those cases. The returned handle is bound to this one's scope.
    pub fn as_subsurface(&self) -> Option<Subsurface<'h>> {
        // SAFETY: the handle borrows a live surface; the downcast reads its
        // role and returns null rather than a wrong object when it is not a
        // sub-surface.
        let raw = unsafe { sys::wlr_subsurface_try_from_wlr_surface(self.as_ptr()) };
        let raw = NonNull::new(raw)?;
        // SAFETY: `raw` is the live sub-surface this surface's role names, and
        // the result is bound to the surface handle that produced it.
        Some(unsafe { Subsurface::from_raw_with_id(raw, self.id()) })
    }
}

#[cfg(test)]
mod tests {
    use super::{Subsurface, SubsurfaceParentState};
    use crate::surface::{Surface, SurfaceId};
    use crate::sys;
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::ptr::NonNull;

    /// A zeroed, heap-allocated `wlr_surface` with an initialised (empty) addon
    /// set, so `find_surface_id` can walk it without dereferencing a null list
    /// head.
    struct ScratchSurface(*mut sys::wlr_surface);

    impl ScratchSurface {
        fn new() -> Self {
            let layout = Layout::new::<sys::wlr_surface>();
            // SAFETY: `wlr_surface` is non-zero-sized.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_surface>();
            assert!(!ptr.is_null(), "allocation failed");
            // SAFETY: `ptr` is sized for the addon set it embeds.
            unsafe { sys::wlr_addon_set_init(&raw mut (*ptr).addons) };
            Self(ptr)
        }

        /// # Safety
        ///
        /// The returned handle borrows this allocation and must not outlive it.
        unsafe fn surface(&self, id: SurfaceId) -> Surface<'_> {
            // SAFETY: the caller upholds the lifetime bound.
            unsafe { Surface::from_raw_with_id(self.0, id) }
        }
    }

    impl Drop for ScratchSurface {
        fn drop(&mut self) {
            // SAFETY: initialised in `new`, no addon left attached (the caller
            // finishes any it attached), so this undoes the init.
            unsafe { sys::wlr_addon_set_finish(&raw mut (*self.0).addons) };
            // SAFETY: allocated with this layout in `new`.
            unsafe { dealloc(self.0.cast::<u8>(), Layout::new::<sys::wlr_surface>()) };
        }
    }

    /// A zeroed, heap-allocated `wlr_subsurface` pointing at a parent surface.
    struct ScratchSubsurface(*mut sys::wlr_subsurface);

    impl ScratchSubsurface {
        fn new(parent: *mut sys::wlr_surface, x: i32, y: i32) -> Self {
            let layout = Layout::new::<sys::wlr_subsurface>();
            // SAFETY: `wlr_subsurface` is non-zero-sized.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_subsurface>();
            assert!(!ptr.is_null(), "allocation failed");
            // SAFETY: `ptr` is a live, exclusively-owned, zeroed object; every
            // field written is in bounds.
            unsafe {
                (*ptr).parent = parent;
                (*ptr).current.x = x;
                (*ptr).current.y = y;
            }
            Self(ptr)
        }

        fn handle(&self, surface: SurfaceId) -> Subsurface<'_> {
            // SAFETY: `ptr` is live for as long as `self`, and the caller
            // upholds the lifetime bound.
            unsafe { Subsurface::from_raw_with_id(NonNull::new(self.0).unwrap(), surface) }
        }
    }

    impl Drop for ScratchSubsurface {
        fn drop(&mut self) {
            // SAFETY: allocated with this layout in `new`.
            unsafe { dealloc(self.0.cast::<u8>(), Layout::new::<sys::wlr_subsurface>()) };
        }
    }

    #[test]
    fn parent_state_reads_the_committed_placement() {
        let parent = ScratchSurface::new();
        let sub = ScratchSubsurface::new(parent.0, 12, -7);
        let handle = sub.handle(SurfaceId(9));
        assert_eq!(handle.surface_id(), SurfaceId(9));
        assert_eq!(
            handle.parent_state(),
            SubsurfaceParentState { x: 12, y: -7 },
            "the committed parent-relative offset is read through"
        );
        assert_eq!(handle.parent_state().x(), 12);
        assert_eq!(handle.parent_state().y(), -7);
    }

    #[test]
    fn parent_surface_id_resolves_only_when_the_parent_is_tracked() {
        let _serial = crate::id::id_test_lock();
        let parent = ScratchSurface::new();
        let sub = ScratchSubsurface::new(parent.0, 0, 0);
        let handle = sub.handle(SurfaceId(3));
        assert_eq!(
            handle.parent_surface_id(),
            None,
            "an untracked parent has no surface id addon"
        );

        // SAFETY: the parent is a live, exclusively-owned scratch surface whose
        // addon set is initialised and empty.
        let parent_id = unsafe { crate::id::attach_surface_id(&raw mut (*parent.0).addons) };
        assert_eq!(
            handle.parent_surface_id(),
            Some(SurfaceId(parent_id)),
            "a tracked parent resolves to its own id"
        );
    }

    #[test]
    fn as_subsurface_misses_on_a_surface_without_the_role() {
        let scratch = ScratchSurface::new();
        // SAFETY: `scratch` outlives the handle.
        let surface = unsafe { scratch.surface(SurfaceId(1)) };
        assert!(
            surface.as_subsurface().is_none(),
            "a surface whose role is null is not a subsurface"
        );
    }
}
