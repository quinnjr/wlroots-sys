//! The `ext_session_lock_v1` surface view.
//!
//! The session-lock lifecycle itself — taking a lock, the security bit, the
//! crash-stays-locked rule — lives on [`Runtime`](crate::Runtime) and in
//! `backend.rs`, because it is a property of the whole session. What was
//! missing is the per-surface view: a compositor that wants to lay out or
//! re-configure a lock surface needs the surface's output and the size wlroots
//! configured it to.
//!
//! [`LockSurface`] is borrow-scoped like [`TearingControl`](crate::TearingControl):
//! wlroots frees the `wlr_session_lock_surface_v1` when its client destroys the
//! protocol object or the lock goes away, neither of which this crate controls.
//! The downcast [`Surface::lock_surface`] uses,
//! `wlr_session_lock_surface_v1_try_from_wlr_surface`, returns null once the
//! lock surface is gone, so a later lookup misses rather than naming freed
//! memory.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::id::find_id;
use crate::output::Output;
use crate::surface::{Surface, SurfaceId};
use crate::{OutputId, Runtime, sys};

/// The size wlroots last configured a lock surface to, plus the serial of that
/// configure.
///
/// An owned snapshot of wlroots' `wlr_session_lock_surface_v1_state`: safe to
/// keep after the handle is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockSurfaceState {
    width: u32,
    height: u32,
    configure_serial: u32,
}

impl LockSurfaceState {
    /// The configured width in surface-local pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The configured height in surface-local pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The serial of the configure that set this size.
    pub fn configure_serial(&self) -> u32 {
        self.configure_serial
    }
}

/// A session lock surface, borrowed for the duration of the [`Surface`] handle
/// it was looked up through.
pub struct LockSurface<'h> {
    raw: NonNull<sys::wlr_session_lock_surface_v1>,
    _scope: PhantomData<&'h ()>,
}

impl std::fmt::Debug for LockSurface<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockSurface")
            .field("configured_size", &self.configured_size())
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl LockSurface<'_> {
    /// Wrap a live lock surface.
    ///
    /// # Safety
    ///
    /// `raw` must be a live `wlr_session_lock_surface_v1` that must not be
    /// destroyed while the returned handle is alive.
    pub(crate) unsafe fn from_non_null<'h>(
        raw: NonNull<sys::wlr_session_lock_surface_v1>,
    ) -> LockSurface<'h> {
        LockSurface {
            raw,
            _scope: PhantomData,
        }
    }

    /// The size wlroots configured the lock surface to, `(width, height)`, in
    /// surface-local pixels.
    ///
    /// `(0, 0)` before the first configure lands, which is what wlroots'
    /// `current` state starts at.
    pub fn configured_size(&self) -> (u32, u32) {
        // SAFETY: the handle borrows a live lock surface for `'h`; `current`
        // is a plain embedded struct, always initialised.
        let current = unsafe { &(*self.raw.as_ptr()).current };
        (current.width, current.height)
    }

    /// The lock surface's last configured size and serial.
    pub fn state(&self) -> LockSurfaceState {
        // SAFETY: as in `configured_size`; `current` is a plain value.
        let current: sys::wlr_session_lock_surface_v1_state =
            unsafe { (*self.raw.as_ptr()).current };
        LockSurfaceState {
            width: current.width,
            height: current.height,
            configure_serial: current.configure_serial,
        }
    }

    /// The output this lock surface covers, when the crate tracks it.
    ///
    /// `None` when wlroots has no output recorded for the surface (it nulls the
    /// pointer when the output dies) or when the output was never registered
    /// with this crate.
    pub fn output(&self) -> Option<Output<'_>> {
        // SAFETY: the handle borrows a live lock surface; `output` is null or a
        // live output pointer.
        let output = unsafe { (*self.raw.as_ptr()).output };
        if output.is_null() {
            return None;
        }
        // SAFETY: `output` is live; `find_id` only reads its addon set.
        let id = unsafe { find_id(&raw const (*output).addons) }?;
        // SAFETY: `output` is a live output and `id` is its own id addon value.
        Some(unsafe { Output::from_raw_with_id(output, OutputId(id)) })
    }
}

impl<'h> Surface<'h> {
    /// This surface's `wlr_session_lock_surface_v1`, if it is one.
    ///
    /// `None` for any surface that is not (or is no longer) a session-lock
    /// surface: wlroots' `wlr_session_lock_surface_v1_try_from_wlr_surface`
    /// returns null for a different role and after the lock surface has been
    /// destroyed. The returned handle borrows this `Surface` and cannot outlive
    /// it, exactly as the other role handles cannot outlive a handler.
    #[must_use]
    pub fn lock_surface(&self) -> Option<LockSurface<'h>> {
        // SAFETY: the handle borrows a live surface; the downcast reads its
        // role and returns null rather than a wrong object when it is not a
        // lock surface, or when wlroots has already freed the role.
        let raw = unsafe { sys::wlr_session_lock_surface_v1_try_from_wlr_surface(self.as_ptr()) };
        NonNull::new(raw).map(|raw| {
            // SAFETY: the downcast returned a non-null live lock surface, and
            // it cannot be freed while the `Surface` borrow that produced it
            // lives.
            unsafe { LockSurface::from_non_null(raw) }
        })
    }
}

impl Runtime {
    /// The [`Surface::lock_surface`] path, resolved from a stored [`SurfaceId`].
    ///
    /// `None` when no live surface has `id`, or when that surface is not a
    /// session-lock surface.
    pub fn lock_surface(&self, id: SurfaceId) -> Option<LockSurface<'_>> {
        self.surface(id)?.lock_surface()
    }
}

#[cfg(test)]
mod tests {
    use super::{LockSurface, LockSurfaceState};
    use crate::surface::SurfaceId;
    use crate::sys;
    use crate::test_support::ScratchSurface;
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::ptr::NonNull;

    /// A zeroed `wlr_session_lock_surface_v1` on the heap.
    struct ScratchLockSurface(*mut sys::wlr_session_lock_surface_v1);

    impl ScratchLockSurface {
        fn new() -> Self {
            let layout = Layout::new::<sys::wlr_session_lock_surface_v1>();
            // SAFETY: the type is non-zero-sized.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_session_lock_surface_v1>();
            assert!(!ptr.is_null(), "allocation failed");
            Self(ptr)
        }
    }

    impl Drop for ScratchLockSurface {
        fn drop(&mut self) {
            // SAFETY: allocated with this layout in `new`.
            unsafe {
                dealloc(
                    self.0.cast::<u8>(),
                    Layout::new::<sys::wlr_session_lock_surface_v1>(),
                )
            };
        }
    }

    /// The configured size and serial read straight off the lock surface's
    /// `current` state, and a null output misses.
    #[test]
    fn state_reads_the_current_configure() {
        let scratch = ScratchLockSurface::new();
        // SAFETY: `scratch` is live and exclusively owned.
        unsafe {
            (*scratch.0).current.width = 1280;
            (*scratch.0).current.height = 720;
            (*scratch.0).current.configure_serial = 42;
        }
        // SAFETY: `scratch` outlives the handle.
        let handle = unsafe {
            LockSurface::from_non_null(NonNull::new(scratch.0).expect("scratch is non-null"))
        };
        assert_eq!(handle.configured_size(), (1280, 720));
        assert_eq!(
            handle.state(),
            LockSurfaceState {
                width: 1280,
                height: 720,
                configure_serial: 42,
            }
        );
        assert!(handle.output().is_none(), "a null output misses");
    }

    /// A surface whose role is null is not a lock surface, so the downcast
    /// misses rather than dereferencing anything.
    #[test]
    fn lock_surface_misses_on_a_surface_without_the_role() {
        let scratch = ScratchSurface::new();
        // SAFETY: `scratch` outlives the handle.
        let surface = unsafe { scratch.surface(SurfaceId(1)) };
        assert!(surface.lock_surface().is_none());
    }
}
