//! Sub-surface roles and the `wlr_subcompositor` global.
//!
//! A `wl_subsurface` gives a client a child surface whose contents and
//! placement are committed by its parent: the sub-surface's position relative
//! to the parent (`wlr_subsurface_parent_state`) is applied when the *parent*
//! commits, not on the child's own commit.
//!
//! # Why this module returns snapshots, never a handle
//!
//! Every other role in this crate gets a borrow-scoped handle whose lifetime is
//! tied to the object it names. A sub-surface cannot: wlroots frees the
//! `wlr_subsurface` when the **parent** surface is destroyed
//! (`subsurface_handle_parent_destroy` → `subsurface_destroy` → `free`), while
//! the child `wlr_surface` survives. A handle scoped to the child surface would
//! therefore outlive the role object and dangle — the child outlives its own
//! role. So the relationship is exposed as owned snapshots computed
//! transiently: [`Surface::subsurface_parent_id`] and
//! [`Surface::subsurface_parent_state`] each downcast with
//! `wlr_subsurface_try_from_wlr_surface`, copy out what they need, and drop the
//! raw pointer before returning. Nothing this module hands back holds a
//! `wlr_subsurface *`.
//!
//! The downcast stays safe after the role is gone: wlroots clears the role
//! resource's user-data (`wl_resource_set_user_data(resource, NULL)`) when it
//! frees the sub-surface, so `wlr_subsurface_try_from_wlr_surface` returns null
//! rather than the freed pointer, and the accessors miss with `None`.
//!
//! The `wlr_subcompositor` global itself is display-owned and created once by
//! [`Runtime::init_graphics`](crate::Runtime::init_graphics); the runtime keeps
//! the pointer it used to discard so a future accessor need not search for it.

use std::ptr::NonNull;

use crate::surface::{Surface, SurfaceId};
use crate::sys;

/// The committed placement of a sub-surface relative to its parent.
///
/// An owned snapshot of wlroots' `wlr_subsurface_parent_state`: the fields are
/// copied out when [`Surface::subsurface_parent_state`] runs, so the value is
/// safe to keep for as long as the caller likes. The sub-surface state is not
/// applied on the child's own commit — wlroots applies the parent's `pending`
/// x/y when the **parent** commits — so this reports the last applied
/// placement.
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

impl Surface<'_> {
    /// The `wlr_subsurface` this surface currently carries, if any.
    ///
    /// The downcast is transient — the returned pointer is used and dropped
    /// inside the calling accessor, never stored — which is the whole point of
    /// this module's snapshot-only design.
    fn subsurface_raw(&self) -> Option<NonNull<sys::wlr_subsurface>> {
        // SAFETY: the handle borrows a live surface; the downcast reads its
        // role and returns null rather than a wrong object when it is not a
        // sub-surface, or when wlroots has already freed the role (it nulls the
        // role resource's user-data on destroy).
        NonNull::new(unsafe { sys::wlr_subsurface_try_from_wlr_surface(self.as_ptr()) })
    }

    /// The id of the parent surface, when this surface is a sub-surface and the
    /// crate tracks the parent.
    ///
    /// `None` for any surface that is not (or is no longer) a sub-surface — a
    /// plain surface, a toplevel, a popup, or a child whose parent has been
    /// destroyed — the same condition under which wlroots'
    /// `wlr_subsurface_try_from_wlr_surface` returns null. Also `None` when the
    /// parent was never registered, since identity comes from the parent's
    /// addon and never from a raw pointer.
    pub fn subsurface_parent_id(&self) -> Option<SurfaceId> {
        let subsurface = self.subsurface_raw()?;
        // SAFETY: the handle borrows the live sub-surface the downcast just
        // returned; its `parent` field points at the live parent surface (or is
        // null once there is none).
        let parent = unsafe { (*subsurface.as_ptr()).parent };
        if parent.is_null() {
            return None;
        }
        // SAFETY: `parent` is live, and `find_surface_id` only reads its addon
        // set.
        unsafe { crate::id::find_surface_id(&raw const (*parent).addons) }.map(SurfaceId)
    }

    /// The sub-surface's committed position relative to its parent, when this
    /// surface is a sub-surface.
    ///
    /// Read straight from wlroots' `wlr_subsurface_parent_state`, so it is the
    /// placement the parent last applied, not anything this crate tracks. `None`
    /// under the same conditions as
    /// [`subsurface_parent_id`](Surface::subsurface_parent_id).
    pub fn subsurface_parent_state(&self) -> Option<SubsurfaceParentState> {
        let subsurface = self.subsurface_raw()?;
        // SAFETY: the handle borrows the live sub-surface the downcast just
        // returned, and `current` is a plain embedded struct (not a pointer)
        // that is always initialised once the role exists.
        let state: &sys::wlr_subsurface_parent_state = unsafe { &(*subsurface.as_ptr()).current };
        Some(SubsurfaceParentState {
            x: state.x,
            y: state.y,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::surface::SurfaceId;
    use crate::test_support::ScratchSurface;

    /// A surface whose role is null is not a sub-surface, so both accessors miss
    /// rather than dereferencing anything. This is the same shape wlroots leaves
    /// the child surface in once the parent is destroyed, which the
    /// client-driven test in `tests/subsurfaces.rs` exercises for real.
    ///
    /// There is deliberately no live-role-null-parent unit test beside this
    /// one: the downcast requires `surface->role` to equal wlroots' private
    /// `subsurface_role` static (not exported, not in the headers) *and* a
    /// live `wl_resource` carrying the sub-surface implementation as
    /// `role_resource`, so a live role is unconstructible without a real
    /// client and display. The closest equivalent is the roleless-parent
    /// integration test in `tests/subsurfaces.rs`, which pins the observable
    /// half of the untracked-parent case (the `(None, Some)` read itself is
    /// unexpressible through the handler API — see that test).
    #[test]
    fn accessors_miss_on_a_surface_without_the_role() {
        let scratch = ScratchSurface::new();
        // SAFETY: `scratch` outlives the handle.
        let surface = unsafe { scratch.surface(SurfaceId(1)) };
        assert!(
            surface.subsurface_parent_id().is_none(),
            "a roleless surface has no subsurface parent"
        );
        assert!(
            surface.subsurface_parent_state().is_none(),
            "and no committed parent-relative placement"
        );
    }
}
