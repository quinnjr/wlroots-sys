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

use std::ffi::c_void;
use std::marker::PhantomData;
use std::os::raw::c_int;
use std::ptr::NonNull;

use crate::id::find_surface_id;
use crate::{Toplevel, sys};

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

/// The xdg-shell role a surface currently carries.
///
/// Read from `wlr_xdg_surface.role` through
/// [`Surface::role`](crate::Surface::role), which first asks
/// `wlr_xdg_surface_try_from_wlr_surface` whether the surface is an
/// xdg-surface at all. A surface with no xdg-surface role (a plain
/// sub-surface, or a surface whose xdg role object has already been destroyed)
/// reports [`SurfaceRole::None`] rather than being an error: "no role" is a
/// perfectly ordinary state, not a failure to look one up.
///
/// `#[non_exhaustive]`: the protocol defines exactly three roles today, but a
/// future wlroots that grows a fourth should not force a breaking change on a
/// match over this value. An unrecognized wire value maps to `None`, the
/// "cannot be interpreted" answer, rather than panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SurfaceRole {
    /// No xdg-surface role: a plain surface, or one whose xdg role is gone.
    None,
    /// An `xdg_toplevel`.
    Toplevel,
    /// An `xdg_popup`.
    Popup,
}

impl SurfaceRole {
    /// Decode `enum wlr_xdg_surface_role`. Values `0`/`1`/`2` are the
    /// protocol's own; anything else is a value this build does not know and
    /// reads as [`SurfaceRole::None`], never a panic — the value is set by
    /// wlroots from a client's request, and a compositor must not abort on it.
    pub(crate) fn from_raw(role: sys::wlr_xdg_surface_role) -> SurfaceRole {
        match role.0 {
            1 => SurfaceRole::Toplevel,
            2 => SurfaceRole::Popup,
            _ => SurfaceRole::None,
        }
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

    /// Build a handle from a raw surface and a known id, or `None` for a null
    /// pointer.
    ///
    /// The non-panicking sibling of [`from_raw_with_id`](Self::from_raw_with_id):
    /// call sites reached from `extern "C"` (the surface-tree walk) or from a
    /// hit-test that may return null use this one, because a panic there
    /// aborts the process. It still carries the same obligation —
    ///
    /// # Safety
    ///
    /// `raw` must be null or a live `wlr_surface` whose addon set carries
    /// `id`, and the returned handle must not outlive the callback it was
    /// created for.
    pub(crate) unsafe fn from_raw_opt(
        raw: *mut sys::wlr_surface,
        id: SurfaceId,
    ) -> Option<Surface<'h>> {
        NonNull::new(raw).map(|raw| Surface {
            raw,
            id,
            tearing_manager: None,
            _scope: PhantomData,
        })
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

    /// The xdg-shell role this surface currently carries, if any.
    ///
    /// A thin read over [`SurfaceRole`]; see that type for the unknown-value
    /// and no-role rules.
    #[must_use]
    pub fn role(&self) -> SurfaceRole {
        // SAFETY: the handle borrows a live surface for its lifetime; the
        // `try_from` is a read and may return null, which is checked.
        unsafe {
            let xdg = sys::wlr_xdg_surface_try_from_wlr_surface(self.raw.as_ptr());
            if xdg.is_null() {
                return SurfaceRole::None;
            }
            SurfaceRole::from_raw((*xdg).role)
        }
    }

    /// Downgrade this generic surface to its [`Toplevel`] role, if it has one.
    ///
    /// `None` for a surface that is not an `xdg_toplevel` (a popup, a layer
    /// surface, a plain sub-surface), for one whose xdg role object has been
    /// destroyed, and — defensively — for one this crate never attached a
    /// toplevel id to. The returned handle borrows this `Surface` and cannot
    /// outlive it, exactly as the role handle itself cannot outlive a handler.
    #[must_use]
    pub fn as_toplevel(&self) -> Option<Toplevel<'_>> {
        Toplevel::from_surface(self)
    }

    /// Call `f` for every surface in this surface's tree, with each surface's
    /// position relative to the root, root first (wlroots' own rendering
    /// order).
    ///
    /// This is the Rust-closure form of `wlr_surface_for_each_surface`: the
    /// callback is a `FnMut`, never a raw C function pointer, so a consumer
    /// never touches `wlr_surface_iterator_func_t`. The handle handed to `f`
    /// is built without a tearing manager (the iterator carries no runtime),
    /// so [`Surface::tearing_hint`] on it reports the no-hint answer; every
    /// other accessor reads the live surface normally.
    ///
    /// wlroots walks the *current* committed tree synchronously inside this
    /// call, so `f` must not destroy any surface it is handed — the same
    /// obligation `Backend::run_all` documents for its handlers, and the
    /// reason this method is not an iterator: an escape-proof closure is what
    /// keeps a destroyed surface from being revisited.
    pub fn for_each_surface(&self, mut f: impl FnMut(&Surface<'_>, i32, i32)) {
        // SAFETY: the handle borrows a live surface; the helper runs
        // `wlr_surface_for_each_surface` synchronously against it, and the
        // closure `f` does not outlive the call.
        unsafe {
            for_each_surface_with(&mut f, |iterate, data| {
                sys::wlr_surface_for_each_surface(self.raw.as_ptr(), iterate, data);
            });
        }
    }
}

/// Run `call` with a monomorphized `wlr_surface_iterator_func_t` trampoline
/// that forwards each visited surface to `f`.
///
/// # Safety
///
/// `call` must invoke the iterator it is handed **synchronously**, exactly
/// once, with a live `wlr_surface` as its first argument, and must pass the
/// `user_data` pointer straight through without retaining it. The surfaces it
/// names must stay alive for the duration of that call; `f` must not destroy
/// any of them.
pub(crate) unsafe fn for_each_surface_with<F>(
    f: &mut F,
    call: impl FnOnce(sys::wlr_surface_iterator_func_t, *mut c_void),
) where
    F: FnMut(&Surface<'_>, i32, i32),
{
    /// The one closure-to-C thunk the whole crate shares. Rebuilt generic per
    /// closure type, so there is no erased vtable and no allocation.
    unsafe extern "C" fn visit<F: FnMut(&Surface<'_>, i32, i32)>(
        surface: *mut sys::wlr_surface,
        sx: c_int,
        sy: c_int,
        data: *mut c_void,
    ) {
        // wlroots never hands this callback null, but a null here would be a
        // crash in an `extern "C"` frame, so it is refused rather than
        // dereferenced. No panic: a panic out of an `extern "C"` frame aborts.
        if surface.is_null() || data.is_null() {
            return;
        }
        // SAFETY: `data` is the `&mut F` `for_each_surface_with` passed, still
        // live because the walk is synchronous; `surface` is live per that
        // function's contract.
        unsafe {
            let f = &mut *data.cast::<F>();
            // A surface the crate never attached an id to is skipped rather
            // than handed a fabricated one: the handle's whole contract is
            // that `id` resolves to *this* surface, and inventing a value
            // would break it. In practice every surface wlroots walks has been
            // through `install_surface_listeners`.
            let Some(id) = find_surface_id(&raw const (*surface).addons).map(SurfaceId) else {
                return;
            };
            let handle = Surface {
                raw: NonNull::new_unchecked(surface),
                id,
                tearing_manager: None,
                _scope: PhantomData,
            };
            f(&handle, sx, sy);
        }
    }

    call(Some(visit::<F>), std::ptr::from_mut(f).cast());
}

#[cfg(test)]
mod tests {
    use super::SurfaceId;
    use crate::test_support::ScratchSurface;

    #[test]
    fn current_size_reads_the_committed_state() {
        let scratch = ScratchSurface::new();
        // SAFETY: `scratch` outlives every use of the handle below.
        unsafe {
            (*scratch.raw).current.width = 640;
            (*scratch.raw).current.height = 480;
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
        let p = scratch.raw;
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
