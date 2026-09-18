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
//! The borrow alone cannot see those client-driven frees, so every accessor
//! re-runs the `try_from` downcast before touching the role: the downcast
//! [`Surface::lock_surface`] uses,
//! `wlr_session_lock_surface_v1_try_from_wlr_surface`, returns null once the
//! lock surface is gone, so a later access misses with `None` rather than
//! naming freed memory. (What the borrow still guarantees is the *surface*:
//! the role cannot outlive the `Surface` handle it was resolved through, and
//! the stored surface pointer is only ever handed back to that same downcast,
//! never dereferenced directly.)

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
///
/// The handle stores the surface — not the role pointer — so every accessor
/// can re-validate liveness before touching the role: resolving once and
/// reading a stored role pointer afterwards would dangle when the locker
/// client destroys the role object while the surface lives.
pub struct LockSurface<'h> {
    surface: NonNull<sys::wlr_surface>,
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
    /// Resolve the lock surface for `surface`, which the caller has already
    /// downcast successfully.
    ///
    /// # Safety
    ///
    /// `surface` must be a live `wlr_surface` borrowed for `'h` whose current
    /// role is a live `wlr_session_lock_surface_v1` — i.e. the
    /// `wlr_session_lock_surface_v1_try_from_wlr_surface` downcast on it just
    /// returned non-null. The handle re-runs that downcast before every
    /// access, so a later client-driven *role* destruction misses rather than
    /// dangles; the *surface* itself must still outlive the handle, exactly as
    /// for every other role handle in this crate.
    pub(crate) unsafe fn from_surface<'h>(surface: NonNull<sys::wlr_surface>) -> LockSurface<'h> {
        LockSurface {
            surface,
            _scope: PhantomData,
        }
    }

    /// The live role object, re-resolved on every access.
    ///
    /// `None` once wlroots has freed the role — the locker client destroyed
    /// the protocol object or the lock went away — because the downcast then
    /// returns null instead of the freed pointer. A `Some` value borrows the
    /// surface for `'h` and is only used inside the calling accessor, never
    /// stored.
    fn live(&self) -> Option<NonNull<sys::wlr_session_lock_surface_v1>> {
        // SAFETY: `surface` is borrowed live for `'h`; the downcast reads its
        // role and returns null rather than a wrong object when the role is
        // gone. The surface itself outliving the handle is the caller's
        // `from_surface` contract, unchanged by this re-validation.
        NonNull::new(unsafe {
            sys::wlr_session_lock_surface_v1_try_from_wlr_surface(self.surface.as_ptr())
        })
    }

    /// The size wlroots configured the lock surface to, `(width, height)`, in
    /// surface-local pixels.
    ///
    /// `None` once the role is gone — precisely, this is
    /// [`state`](Self::state)'s `None`: the size is read through the state
    /// snapshot so one accessor owns the field reads. `(0, 0)` inside the
    /// `Some` before the first configure lands, which is what wlroots'
    /// `current` state starts at.
    pub fn configured_size(&self) -> Option<(u32, u32)> {
        self.state().map(|state| (state.width(), state.height()))
    }

    /// The lock surface's last configured size and serial.
    ///
    /// `None` once the role is gone: the downcast misses instead of reading a
    /// freed `current`.
    pub fn state(&self) -> Option<LockSurfaceState> {
        let live = self.live()?;
        // SAFETY: `live` is the role object the downcast just returned, and
        // `current` is a plain embedded struct (not a pointer) that is always
        // initialised once the role exists.
        let current: sys::wlr_session_lock_surface_v1_state = unsafe { (*live.as_ptr()).current };
        Some(LockSurfaceState {
            width: current.width,
            height: current.height,
            configure_serial: current.configure_serial,
        })
    }

    /// The output this lock surface covers, when the crate tracks it.
    ///
    /// `None` when the role is gone (the downcast misses before anything is
    /// read), when wlroots has no output recorded for the surface (it nulls
    /// the pointer when the output dies), or when the output was never
    /// registered with this crate.
    pub fn output(&self) -> Option<Output<'_>> {
        let live = self.live()?;
        // SAFETY: `live` is the role object the downcast just returned;
        // `output` is null or a live output pointer.
        let output = unsafe { (*live.as_ptr()).output };
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
        let is_lock_surface =
            !unsafe { sys::wlr_session_lock_surface_v1_try_from_wlr_surface(self.as_ptr()) }
                .is_null();
        if !is_lock_surface {
            return None;
        }
        // SAFETY: the downcast just returned non-null for this live surface,
        // which is `from_surface`'s contract. Every later access re-runs the
        // downcast, so a role destroyed after this point misses there instead
        // of dangling here.
        Some(unsafe {
            LockSurface::from_surface(
                NonNull::new(self.as_ptr()).expect("a live Surface is non-null"),
            )
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
    use super::LockSurface;
    use crate::surface::SurfaceId;
    use crate::sys;
    use crate::test_support::{Scratch, ScratchSurface};
    use std::ptr::NonNull;

    /// A no-op fini for scratch objects nothing needs to undo, exactly as in
    /// `tearing.rs`.
    fn noop_fini<T>(_: *mut T) {}

    /// A zeroed `wlr_session_lock_surface_v1`, shared via
    /// [`Scratch`](crate::test_support::Scratch) exactly as `tearing.rs`
    /// shares its manager/control objects. Zeroed is a valid initial state
    /// here: no accessor touches the struct's fields without a live role, and
    /// the downcast misses before any field is read.
    fn new_lock_surface() -> Scratch<sys::wlr_session_lock_surface_v1> {
        Scratch::new(|_| {}, noop_fini)
    }

    /// Initialise a zeroed scratch output's addon set, so `find_id` may read
    /// it. The set stays empty — registered nowhere, carrying no id addon.
    fn init_output_addons(ptr: *mut sys::wlr_output) {
        // SAFETY: `ptr` is a live, exclusively-owned, zeroed output; the
        // addon set is in bounds and untouched.
        unsafe { sys::wlr_addon_set_init(&raw mut (*ptr).addons) };
    }

    /// Undo exactly what [`init_output_addons`] did.
    fn fini_output_addons(ptr: *mut sys::wlr_output) {
        // SAFETY: the addon set was initialised in `init_output_addons` and
        // nothing else was attached to it.
        unsafe { sys::wlr_addon_set_finish(&raw mut (*ptr).addons) };
    }

    /// A live output with an initialised but empty addon set: `find_id` may
    /// read it, and finds no id because the output was never registered.
    fn new_unregistered_output() -> Scratch<sys::wlr_output> {
        Scratch::new(init_output_addons, fini_output_addons)
    }

    /// Resolve `surface` as a lock surface. The scratch surface carries no
    /// role, so resolution itself succeeds structurally (the handle only
    /// stores the surface) while every accessor must miss at the re-validated
    /// downcast.
    ///
    /// # Safety
    ///
    /// `surface` must outlive the returned handle.
    unsafe fn resolve_on(surface: &ScratchSurface) -> LockSurface<'_> {
        // SAFETY: the caller upholds the lifetime bound; the scratch surface
        // allocation is non-null.
        unsafe {
            LockSurface::from_surface(
                NonNull::new(surface.raw).expect("scratch surface is non-null"),
            )
        }
    }

    /// A resolved handle whose role is gone misses on every accessor instead
    /// of touching freed memory.
    ///
    /// The scratch surface carries no role, which is exactly the observable
    /// state wlroots leaves behind a destroyed role: on destroy it nulls the
    /// role resource's user-data, so `try_from` returns null for a destroyed
    /// role and for a never-roled surface alike (the same equivalence
    /// `subsurface.rs` documents). A pre-fix handle — one that read a stored
    /// role pointer directly — would dereference whatever the freed struct's
    /// memory now holds; the re-validating accessors return `None`.
    #[test]
    fn accessors_miss_once_the_role_is_gone() {
        let surface = ScratchSurface::new();
        // SAFETY: `surface` outlives the handle.
        let handle = unsafe { resolve_on(&surface) };
        assert_eq!(
            handle.configured_size(),
            None,
            "no role, no configured size"
        );
        assert_eq!(handle.state(), None, "no role, no state snapshot");
        assert!(handle.output().is_none(), "no role, no output");
    }

    /// A scratch lock surface wired to a live but unregistered output still
    /// misses: the `None` fires at the liveness re-validation above, before
    /// the output pointer is ever read.
    ///
    /// The output arm itself (`find_id` finding no id addon) needs a live
    /// role to reach, so it is client-driven by construction — same constraint
    /// `subsurface.rs` documents for its live-role cases. The fixture still
    /// wires a live unregistered output (rather than a null) so that arm, and
    /// not the null-output arm, is what a future live-role harness would hit.
    #[test]
    fn output_misses_on_a_live_but_unregistered_output() {
        let surface = ScratchSurface::new();
        let output = new_unregistered_output();
        let lock = new_lock_surface();
        // SAFETY: both scratch objects are live and exclusively owned.
        unsafe {
            (*lock.ptr).output = output.ptr;
        }
        // SAFETY: `surface` outlives the handle.
        let handle = unsafe { resolve_on(&surface) };
        assert!(
            handle.output().is_none(),
            "an unregistered output resolves to no tracked output"
        );
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
