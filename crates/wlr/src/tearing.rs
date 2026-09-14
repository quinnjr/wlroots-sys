//! Per-surface tearing hints from `wp_tearing_control_v1`.
//!
//! A client that wants to trade tearing for latency can ask the compositor to
//! let it present asynchronously: it binds `wp_tearing_control_manager_v1`,
//! creates a `wp_tearing_control_v1` for one surface, and sets a presentation
//! hint (vsync or async). wlroots keeps that object attached to the surface and
//! applies the hint at commit time; the compositor reads the *effective* hint
//! with [`Surface::tearing_hint`] before deciding whether a frame may tear.
//!
//! Two views are wrapped here. [`Surface::tearing_hint`] is the one a
//! rendering path wants — a plain value, defaulting to
//! [`TearingHint::Vsync`] when no client has asked for anything, exactly as
//! wlroots' own lookup does. [`Surface::tearing_control`] is the object itself,
//! borrowed for as long as the surface handle lives, for a compositor that
//! wants to observe the pending hint (what the client asked for but has not yet
//! committed) alongside the current one.
//!
//! The manager is created per display by
//! [`Runtime::create_tearing_control_manager`]; a runtime that never creates it
//! has no hint to read, and both accessors miss with `None`.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::surface::{Surface, SurfaceId};
use crate::{Runtime, sys};

/// The presentation hint a client set on its surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TearingHint {
    /// Present in step with the display's vertical blank. The default when no
    /// client has asked for anything.
    Vsync,
    /// Present as soon as the buffer is ready, even if that tears.
    Async,
}

impl TearingHint {
    /// Map wlroots' enum to this crate's.
    ///
    /// Anything that is not the async constant maps to [`TearingHint::Vsync`],
    /// matching `wlr_tearing_control_manager_v1_surface_hint_from_surface`'s
    /// own default for an unknown surface. The C enum has exactly two values in
    /// this protocol version, so the wildcard only covers a future one.
    fn from_raw(raw: sys::wp_tearing_control_v1_presentation_hint) -> TearingHint {
        if raw
            == sys::wp_tearing_control_v1_presentation_hint::WP_TEARING_CONTROL_V1_PRESENTATION_HINT_ASYNC
        {
            TearingHint::Async
        } else {
            TearingHint::Vsync
        }
    }
}

/// A client's `wp_tearing_control_v1`, borrowed for the duration of the
/// surface handle it was looked up through.
///
/// wlroots frees the object when its client destroys the protocol object, the
/// surface dies, or the manager is torn down with the display — none of which
/// this handle controls — so it is borrow-scoped like every other handle in
/// this crate rather than owned. The surface id it names is the storable value.
pub struct TearingControl<'h> {
    raw: NonNull<sys::wlr_tearing_control_v1>,
    _scope: PhantomData<&'h ()>,
}

impl std::fmt::Debug for TearingControl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TearingControl")
            .field("surface_id", &self.surface_id())
            .field("current", &self.current_hint())
            .field("pending", &self.pending_hint())
            .finish_non_exhaustive()
    }
}

impl<'h> TearingControl<'h> {
    /// Wrap a live tearing-control object.
    ///
    /// `assert`/`unwrap`-free by construction: the only producer, the manager
    /// list walk in [`Surface::tearing_control`], already holds a non-null
    /// pointer, so this takes the `NonNull` rather than re-checking a raw one.
    pub(crate) fn from_non_null(raw: NonNull<sys::wlr_tearing_control_v1>) -> TearingControl<'h> {
        TearingControl {
            raw,
            _scope: PhantomData,
        }
    }

    /// The hint in effect: what the client last committed.
    pub fn current_hint(&self) -> TearingHint {
        // SAFETY: the handle borrows a live tearing-control object for `'h`.
        TearingHint::from_raw(unsafe { (*self.raw.as_ptr()).current })
    }

    /// The hint the client has asked for but not yet committed.
    ///
    /// wlroots only moves this into [`current_hint`](Self::current_hint) on the
    /// surface's next commit, so mid-fame the two can disagree.
    pub fn pending_hint(&self) -> TearingHint {
        // SAFETY: as in `current_hint`. `pending` is a plain field.
        TearingHint::from_raw(unsafe { (*self.raw.as_ptr()).pending })
    }

    /// The hint in effect before the most recent commit.
    ///
    /// Read from the private state wlroots keeps to decide whether a commit
    /// changed the hint. Use it for change detection, not for rendering.
    pub fn previous_hint(&self) -> TearingHint {
        // SAFETY: as in `current_hint`. `WLR_PRIVATE.previous` is a plain field
        // inside the same live object.
        TearingHint::from_raw(unsafe { (*self.raw.as_ptr()).WLR_PRIVATE.previous })
    }

    /// The id of the surface this control applies to, when the crate tracks it.
    ///
    /// `None` when the surface was never registered with this crate, which is
    /// the same miss [`SurfaceId`] lookups elsewhere report.
    pub fn surface_id(&self) -> Option<SurfaceId> {
        // SAFETY: the handle borrows a live control; its `surface` field points
        // at the live surface the object is attached to (or is null).
        let surface = unsafe { (*self.raw.as_ptr()).surface };
        if surface.is_null() {
            return None;
        }
        // SAFETY: `surface` is live, and `find_surface_id` only reads its addon
        // set.
        unsafe { crate::id::find_surface_id(&raw const (*surface).addons) }.map(SurfaceId)
    }
}

impl<'h> Surface<'h> {
    /// The tearing hint in effect for this surface.
    ///
    /// `None` when no `wp_tearing_control_manager_v1` was created on this
    /// runtime, since there is then no hint to read. When the manager exists
    /// but this surface has no client-created control object, wlroots reports
    /// [`TearingHint::Vsync`], so this is `Some(Vsync)` rather than `None`.
    pub fn tearing_hint(&self) -> Option<TearingHint> {
        let manager = self.tearing_manager()?;
        // SAFETY: the manager is live (it is display-owned and the handle's
        // runtime borrow keeps the display alive), and the surface is live. The
        // call reads the surface's addon set and returns a plain enum.
        let raw = unsafe {
            sys::wlr_tearing_control_manager_v1_surface_hint_from_surface(
                manager.as_ptr(),
                self.as_ptr(),
            )
        };
        Some(TearingHint::from_raw(raw))
    }

    /// The client's tearing-control object for this surface, if one exists.
    ///
    /// Found by walking the manager's `surface_hints` list, the only way
    /// wlroots exposes a surface's control object. The list is modified only by
    /// wlroots, and this walk neither dispatches the event loop nor calls into
    /// wlroots, so no entry can be freed underneath it.
    pub fn tearing_control(&self) -> Option<TearingControl<'h>> {
        let manager = self.tearing_manager()?;
        let surface = self.as_ptr();
        // SAFETY: the manager is live and its `surface_hints` list initialised,
        // as it is from creation; every linked entry is a live
        // `wlr_tearing_control_v1`, and the list is not disturbed during the
        // walk.
        for control in unsafe {
            sys::wl_list_for_each!(
                &raw mut (*manager.as_ptr()).surface_hints,
                sys::wlr_tearing_control_v1,
                link
            )
        } {
            // SAFETY: `control` is a live entry of the manager's list.
            if unsafe { (*control).surface } == surface {
                return NonNull::new(control).map(TearingControl::from_non_null);
            }
        }
        None
    }
}

impl Runtime {
    /// The [`Surface::tearing_hint`] path, resolved from a stored [`SurfaceId`].
    ///
    /// `None` when no live surface has `id`, or when no tearing-control manager
    /// was created — the by-id miss and the missing-global miss, both explicit.
    pub fn tearing_hint(&self, id: SurfaceId) -> Option<TearingHint> {
        self.surface(id)?.tearing_hint()
    }

    /// The [`Surface::tearing_control`] path, resolved from a stored
    /// [`SurfaceId`].
    ///
    /// `None` when no live surface has `id`, when no manager was created, or
    /// when that surface's client never created a control object.
    pub fn tearing_control(&self, id: SurfaceId) -> Option<TearingControl<'_>> {
        self.surface(id)?.tearing_control()
    }
}

#[cfg(test)]
mod tests {
    use super::{TearingControl, TearingHint};
    use crate::surface::{Surface, SurfaceId};
    use crate::sys;
    use crate::test_support::ScratchSurface;
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::ptr::NonNull;

    /// A zeroed manager whose `surface_hints` list is initialised to empty.
    struct ScratchManager(*mut sys::wlr_tearing_control_manager_v1);

    impl ScratchManager {
        fn new() -> Self {
            let layout = Layout::new::<sys::wlr_tearing_control_manager_v1>();
            // SAFETY: the manager is non-zero-sized.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_tearing_control_manager_v1>();
            assert!(!ptr.is_null(), "allocation failed");
            // SAFETY: `ptr` is sized for the list it embeds; an empty list
            // points at itself.
            unsafe {
                (*ptr).surface_hints.prev = &raw mut (*ptr).surface_hints;
                (*ptr).surface_hints.next = &raw mut (*ptr).surface_hints;
            }
            Self(ptr)
        }

        /// Link `control` at the head of the list. No unlink on drop: these
        /// scratch objects are freed wholesale and nothing reads the list after.
        ///
        /// # Safety
        ///
        /// `control` must be a live control whose `link` is not already linked.
        unsafe fn link(&mut self, control: *mut sys::wlr_tearing_control_v1) {
            // SAFETY: `self.0` is live and its list initialised; `control` is
            // live per the caller.
            unsafe {
                (*control).link.prev = &raw mut (*self.0).surface_hints;
                (*control).link.next = (*self.0).surface_hints.next;
                (*(*self.0).surface_hints.next).prev = &raw mut (*control).link;
                (*self.0).surface_hints.next = &raw mut (*control).link;
            }
        }
    }

    impl Drop for ScratchManager {
        fn drop(&mut self) {
            // SAFETY: allocated with this layout in `new`.
            unsafe {
                dealloc(
                    self.0.cast::<u8>(),
                    Layout::new::<sys::wlr_tearing_control_manager_v1>(),
                )
            };
        }
    }

    /// A zeroed control on the heap.
    struct ScratchControl(*mut sys::wlr_tearing_control_v1);

    impl ScratchControl {
        fn new(
            surface: *mut sys::wlr_surface,
            current: TearingHint,
            pending: TearingHint,
            previous: TearingHint,
        ) -> Self {
            let layout = Layout::new::<sys::wlr_tearing_control_v1>();
            // SAFETY: the control is non-zero-sized.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_tearing_control_v1>();
            assert!(!ptr.is_null(), "allocation failed");
            // SAFETY: `ptr` is a live, exclusively-owned, zeroed control; every
            // field written is in bounds. The hints are converted to the C
            // constants by hand so the test does not depend on `from_raw`.
            unsafe {
                (*ptr).surface = surface;
                (*ptr).current = hint_to_raw(current);
                (*ptr).pending = hint_to_raw(pending);
                (*ptr).WLR_PRIVATE.previous = hint_to_raw(previous);
            }
            Self(ptr)
        }
    }

    impl Drop for ScratchControl {
        fn drop(&mut self) {
            // SAFETY: allocated with this layout in `new`.
            unsafe {
                dealloc(
                    self.0.cast::<u8>(),
                    Layout::new::<sys::wlr_tearing_control_v1>(),
                )
            };
        }
    }

    fn hint_to_raw(hint: TearingHint) -> sys::wp_tearing_control_v1_presentation_hint {
        match hint {
            TearingHint::Async => {
                sys::wp_tearing_control_v1_presentation_hint::WP_TEARING_CONTROL_V1_PRESENTATION_HINT_ASYNC
            }
            TearingHint::Vsync => {
                sys::wp_tearing_control_v1_presentation_hint::WP_TEARING_CONTROL_V1_PRESENTATION_HINT_VSYNC
            }
        }
    }

    #[test]
    fn hint_lookup_defaults_to_vsync_without_a_control_object() {
        let surface_scratch = ScratchSurface::new();
        let manager = ScratchManager::new();
        // SAFETY: both scratch objects outlive the handle.
        let surface = unsafe { Surface::from_raw_with_id(surface_scratch.raw, SurfaceId(1)) }
            .with_tearing_manager(Some(
                NonNull::new(manager.0).expect("scratch manager is non-null"),
            ));

        assert_eq!(
            surface.tearing_hint(),
            Some(TearingHint::Vsync),
            "wlroots reports vsync when the surface has no control object"
        );
        assert!(
            surface.tearing_control().is_none(),
            "and there is no object to hand back"
        );
    }

    #[test]
    fn hint_lookup_misses_without_a_manager() {
        let surface_scratch = ScratchSurface::new();
        // SAFETY: the scratch surface outlives the handle.
        let surface = unsafe { Surface::from_raw_with_id(surface_scratch.raw, SurfaceId(1)) };
        assert_eq!(surface.tearing_hint(), None, "no manager, no hint");
        assert!(surface.tearing_control().is_none());
    }

    #[test]
    fn control_walk_finds_the_object_for_this_surface() {
        let surface_scratch = ScratchSurface::new();
        let other_scratch = ScratchSurface::new();
        let mut manager = ScratchManager::new();
        let control = ScratchControl::new(
            surface_scratch.raw,
            TearingHint::Async,
            TearingHint::Vsync,
            TearingHint::Vsync,
        );
        let other = ScratchControl::new(
            other_scratch.raw,
            TearingHint::Vsync,
            TearingHint::Async,
            TearingHint::Vsync,
        );
        // SAFETY: both controls are live and their links unlinked; the manager
        // outlives the walk.
        unsafe {
            manager.link(other.0);
            manager.link(control.0);
        }

        // SAFETY: the scratch objects outlive the handle.
        let surface = unsafe { Surface::from_raw_with_id(surface_scratch.raw, SurfaceId(1)) }
            .with_tearing_manager(Some(
                NonNull::new(manager.0).expect("scratch manager is non-null"),
            ));
        let found = surface
            .tearing_control()
            .expect("the surface's own control is in the list");
        assert_eq!(found.current_hint(), TearingHint::Async);
        assert_eq!(found.pending_hint(), TearingHint::Vsync);
        assert_eq!(found.previous_hint(), TearingHint::Vsync);
        assert_eq!(
            found.surface_id(),
            None,
            "the scratch surface has no id addon"
        );

        // The lookup is keyed by surface, not by list position: the other
        // surface in the same manager resolves to its own (vsync) control.
        let other_surface = unsafe { Surface::from_raw_with_id(other_scratch.raw, SurfaceId(2)) }
            .with_tearing_manager(Some(
                NonNull::new(manager.0).expect("scratch manager is non-null"),
            ));
        let other_found = other_surface
            .tearing_control()
            .expect("the other surface's control is in the list");
        assert_eq!(
            other_found.current_hint(),
            TearingHint::Vsync,
            "the vsync control belongs to the other surface"
        );
        assert_eq!(other_found.pending_hint(), TearingHint::Async);
    }

    #[test]
    fn control_accessors_read_the_current_pending_and_previous_hints() {
        let surface_scratch = ScratchSurface::new();
        let control = ScratchControl::new(
            surface_scratch.raw,
            TearingHint::Vsync,
            TearingHint::Async,
            TearingHint::Async,
        );
        // SAFETY: `control` is live and this is its only handle.
        let handle = TearingControl::from_non_null(
            NonNull::new(control.0).expect("scratch control is non-null"),
        );
        assert_eq!(handle.current_hint(), TearingHint::Vsync);
        assert_eq!(handle.pending_hint(), TearingHint::Async);
        assert_eq!(handle.previous_hint(), TearingHint::Async);
    }
}
