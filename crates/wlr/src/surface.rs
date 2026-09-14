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
//!
//! `surface_committed` takes `&Surface` because a commit fires while the
//! surface is synchronously live and its new state is worth reading, while
//! `surface_mapped`/`surface_unmapped`/`surface_destroyed` take `SurfaceId`
//! because they name a recorded identity that may already be gone.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::os::raw::c_int;
use std::ptr::NonNull;
use std::time::Duration;

use crate::geom::{Box2D, FBox, Transform};
use crate::id::{find_id, find_surface_id};
use crate::region::Region;
use crate::{LayerSurface, LayerSurfaceId, Output, Toplevel, sys};

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

/// A held [`Surface::lock_pending`](crate::Surface::lock_pending), released by
/// handing it back to [`Surface::unlock_cached`](crate::Surface::unlock_cached).
///
/// Opaque, and neither `Clone` nor `Copy`. wlroots'
/// `wlr_surface_unlock_cached` aborts when a state is unlocked with no matching
/// lock, so the type system is the first guard: a value only `lock_pending` can
/// mint, consumed on release. The second is the recorded surface identity:
/// `unlock_cached` compares it before the call and refuses a token minted by a
/// different surface, which is otherwise reachable because two handles from one
/// [`Runtime`](crate::Runtime) share a scope lifetime. The lifetime ties the
/// token to the scope that produced it, and the pointer makes it `!Send`/`!Sync`.
#[derive(Debug)]
#[must_use = "a pending lock must be released or the surface stops committing"]
pub struct PendingLock<'h> {
    seq: u32,
    surface: *mut sys::wlr_surface,
    _scope: PhantomData<&'h ()>,
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
    /// The runtime's `wlr_seat`, cached for the same reason as the tearing
    /// manager: [`Surface::accepts_touch`](crate::Surface::accepts_touch) is a
    /// query against the compositor's own seat, and a handle can only reach it
    /// if its builder put it here. `None` for a handle built without a runtime
    /// (the surface-tree walk, the scratch constructor), where the query
    /// answers `false` rather than guessing.
    seat: Option<NonNull<sys::wlr_seat>>,
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
            seat: None,
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
            seat: None,
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

    /// Attach the runtime's seat, so [`accepts_touch`](Surface::accepts_touch)
    /// can reach it. Consuming builder, mirroring
    /// [`with_tearing_manager`](Surface::with_tearing_manager) and for the same
    /// reason.
    pub(crate) fn with_seat(mut self, seat: Option<NonNull<sys::wlr_seat>>) -> Surface<'h> {
        self.seat = seat;
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

    /// The union of this surface and every sub-surface's current extent, in
    /// surface-local coordinates.
    ///
    /// `x`/`y` may be negative, because a sub-surface may sit at a negative
    /// offset. An uncommitted surface reports all-zero, the same as
    /// [`current_size`](Surface::current_size).
    #[must_use]
    pub fn extents(&self) -> Box2D {
        let mut box_ = sys::wlr_box {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        // SAFETY: the handle borrows a live surface; `box_` is a live local
        // wlroots writes into and this method then reads.
        unsafe { sys::wlr_surface_get_extents(self.raw.as_ptr(), &raw mut box_) };
        Box2D::new(box_.x, box_.y, box_.width, box_.height)
    }

    /// The effective damage of the last commit, in surface-local coordinates.
    ///
    /// This is wlroots' own accumulated damage — client surface damage merged
    /// with buffer damage and clipped — not the raw client-stated region.
    #[must_use]
    pub fn effective_damage(&self) -> Region {
        let mut region = Region::new();
        // SAFETY: the handle borrows a live surface; `region` is a live,
        // initialised pixman region wlroots writes into.
        unsafe {
            sys::wlr_surface_get_effective_damage(self.raw.as_ptr(), region.as_mut_ptr());
        }
        region
    }

    /// The region of the attached buffer that has to be sampled to render this
    /// surface, in buffer-local coordinates.
    ///
    /// A surface with no viewport set — the ordinary case — reports the whole
    /// buffer. An uncommitted surface reports an empty box.
    #[must_use]
    pub fn buffer_source_box(&self) -> FBox {
        let mut box_ = sys::wlr_fbox {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        };
        // SAFETY: as for `extents`: a live surface and a live out-parameter.
        unsafe { sys::wlr_surface_get_buffer_source_box(self.raw.as_ptr(), &raw mut box_) };
        FBox::new(box_.x, box_.y, box_.width, box_.height)
    }

    /// This surface's stable id at the root of its sub-surface tree.
    ///
    /// wlroots returns the surface itself when it is already the root, and
    /// never returns null. A root this crate never tracked — impossible for a
    /// surface it announced, possible only for a scratch handle — falls back to
    /// this handle's own id rather than inventing one.
    #[must_use]
    pub fn root_id(&self) -> SurfaceId {
        // SAFETY: the handle borrows a live surface; `get_root_surface` only
        // reads and never returns null for a live surface.
        unsafe {
            let root = sys::wlr_surface_get_root_surface(self.raw.as_ptr());
            if root.is_null() || root == self.raw.as_ptr() {
                return self.id;
            }
            find_surface_id(&raw const (*root).addons)
                .map(SurfaceId)
                .unwrap_or(self.id)
        }
    }

    /// Whether this surface accepts an input event at the given surface-local
    /// point.
    ///
    /// Consults only this surface's own input region, not its sub-surfaces';
    /// [`surface_at`](Surface::surface_at) is the tree-wide hit-test.
    #[must_use]
    pub fn point_accepts_input(&self, sx: f64, sy: f64) -> bool {
        // SAFETY: the handle borrows a live surface; the call only reads it.
        unsafe { sys::wlr_surface_point_accepts_input(self.raw.as_ptr(), sx, sy) }
    }

    /// Hit-test this surface's whole tree at a point in the root's
    /// surface-local coordinates.
    ///
    /// Returns the leaf surface the point lands on, and the point in that
    /// leaf's own coordinates; `None` for a miss. Mirrors
    /// [`Toplevel::surface_at`](crate::Toplevel::surface_at).
    ///
    /// The returned leaf is built without this handle's cached seat, so
    /// [`accepts_touch`](Surface::accepts_touch) on it always reports `false`.
    /// The leaf is a hit-test result, not a new compositor context; call
    /// `accepts_touch` on the surface that carries the seat (the handler's own
    /// handle, or [`Runtime::surface`](crate::Runtime::surface)).
    #[must_use]
    pub fn surface_at(&self, sx: f64, sy: f64) -> Option<(Surface<'_>, f64, f64)> {
        self.surface_at_impl(sys::wlr_surface_surface_at, sx, sy)
    }

    /// Shared body of the surface-tree hit-tests, which differ only in which
    /// wlroots walk they call.
    fn surface_at_impl(
        &self,
        walk: unsafe extern "C" fn(
            *mut sys::wlr_surface,
            f64,
            f64,
            *mut f64,
            *mut f64,
        ) -> *mut sys::wlr_surface,
        sx: f64,
        sy: f64,
    ) -> Option<(Surface<'_>, f64, f64)> {
        let mut sub_x = 0.0;
        let mut sub_y = 0.0;
        // SAFETY: the handle borrows a live surface; both out-parameters are
        // live locals that outlive the call, and wlroots only reads the
        // coordinates.
        unsafe {
            let raw = walk(self.raw.as_ptr(), sx, sy, &raw mut sub_x, &raw mut sub_y);
            if raw.is_null() {
                return None;
            }
            let id = find_surface_id(&raw const (*raw).addons).map(SurfaceId)?;
            let surface = Surface::from_raw_opt(raw, id)?;
            Some((surface, sub_x, sub_y))
        }
    }

    /// Whether the client behind this surface has bound touch on the
    /// compositor's seat.
    ///
    /// `false` when the crate has no seat (no `Runtime::create_seat` ran) and
    /// for a handle built outside a runtime, rather than a wrong answer: no
    /// seat means no touch can be delivered.
    #[must_use]
    pub fn accepts_touch(&self) -> bool {
        let Some(seat) = self.seat else {
            return false;
        };
        // SAFETY: the handle borrows a live surface and the seat was cached
        // from the runtime it came from, so both are live for this call.
        unsafe { sys::wlr_surface_accepts_touch(self.raw.as_ptr(), seat.as_ptr()) }
    }

    /// Tell the client this surface entered `output`.
    ///
    /// A no-op when the surface has already entered it. wlroots sends the
    /// `wl_surface.enter` event only to the client's matching `wl_output`
    /// resources, so this is client-visible only when the client bound the
    /// output global.
    pub fn send_enter(&self, output: &Output<'_>) {
        // SAFETY: the handle borrows a live surface and `output` a live output;
        // wlroots only reads both.
        unsafe { sys::wlr_surface_send_enter(self.raw.as_ptr(), output.as_ptr()) };
    }

    /// Tell the client this surface left `output`. The mirror of
    /// [`send_enter`](Surface::send_enter), and a no-op when it never entered.
    pub fn send_leave(&self, output: &Output<'_>) {
        // SAFETY: as for `send_enter`.
        unsafe { sys::wlr_surface_send_leave(self.raw.as_ptr(), output.as_ptr()) };
    }

    /// Complete this surface's queued frame callbacks, telling the client now
    /// is a good time to draw again.
    ///
    /// `when` is the presentation timestamp the callback carries, on the same
    /// monotonic clock [`Runtime::send_scene_surface_frame_done`](crate::Runtime::send_scene_surface_frame_done)
    /// uses.
    pub fn send_frame_done(&self, when: Duration) {
        let now = crate::scene::timespec_of(when);
        // SAFETY: the handle borrows a live surface; `now` is a live local
        // wlroots reads out of.
        unsafe { sys::wlr_surface_send_frame_done(self.raw.as_ptr(), &raw const now) };
    }

    /// Ask the client to use `scale` for buffers on this surface.
    ///
    /// Sends a `wl_surface.preferred_buffer_scale` event; the client is free to
    /// ignore it. `None` for a nonpositive `scale`, which wlroots'
    /// `wlr_surface_set_preferred_buffer_scale` rejects with an `assert` — and
    /// this distribution ships wlroots without `NDEBUG`, so forwarding it would
    /// abort the whole compositor rather than merely fail. A compositor must
    /// never hand a client's bad scale straight through.
    pub fn set_preferred_buffer_scale(&self, scale: i32) -> Option<()> {
        if scale <= 0 {
            return None;
        }
        // SAFETY: the handle borrows a live surface; the call only reads it and
        // sends on its resource when one exists, and `scale` is positive as its
        // own assert requires.
        unsafe { sys::wlr_surface_set_preferred_buffer_scale(self.raw.as_ptr(), scale) };
        Some(())
    }

    /// Ask the client to use `transform` for buffers on this surface.
    ///
    /// Sends a `wl_surface.preferred_buffer_transform` event; the client is
    /// free to ignore it.
    pub fn set_preferred_buffer_transform(&self, transform: Transform) {
        // SAFETY: as for `set_preferred_buffer_scale`.
        unsafe {
            sys::wlr_surface_set_preferred_buffer_transform(self.raw.as_ptr(), transform.into());
        }
    }

    /// Lock this surface's pending state, returning a token that releases it.
    ///
    /// While locked the pending state is not committed but cached; every lock
    /// must be released through [`unlock_cached`](Surface::unlock_cached) or
    /// the surface stops committing. The token is deliberately opaque and
    /// single-use: wlroots' `wlr_surface_unlock_cached` aborts when a state is
    /// unlocked with no matching lock, so safe code must not be able to invent
    /// a sequence number or release the same lock twice.
    pub fn lock_pending(&self) -> PendingLock<'h> {
        // SAFETY: the handle borrows a live surface; the call only touches its
        // own state.
        PendingLock {
            seq: unsafe { sys::wlr_surface_lock_pending(self.raw.as_ptr()) },
            surface: self.raw.as_ptr(),
            _scope: PhantomData,
        }
    }

    /// Release one [`lock_pending`](Surface::lock_pending) on this surface.
    ///
    /// The state is not guaranteed to commit immediately, because another lock
    /// may still be outstanding. `None` when `lock` was minted by a different
    /// surface: two handles can share a scope lifetime, so the check is what
    /// keeps a cross-surface replay from reaching wlroots' assert — the token
    /// embeds the issuing surface's address and this method refuses any other.
    pub fn unlock_cached(&self, lock: PendingLock<'h>) -> Option<()> {
        if lock.surface != self.raw.as_ptr() {
            return None;
        }
        // SAFETY: `lock` names a lock this exact surface took (the identity
        // check above), so wlroots' `cached_state_locks > 0` assert holds; the
        // handle borrows a live surface.
        unsafe { sys::wlr_surface_unlock_cached(self.raw.as_ptr(), lock.seq) };
        Some(())
    }

    /// Unmap this surface, dropping it from the screen.
    ///
    /// **A surface-role implementation operation**, by wlroots' own contract:
    /// it must only be used by surface role implementations. This crate exposes
    /// it because it is the only way to force a mapped surface off screen from
    /// outside a role, not because it is an ordinary compositor operation.
    /// Idempotent; wlroots emits the surface's unmap event — and runs the
    /// role's own `unmap` hook — which this crate forwards as
    /// [`ToplevelHandler::surface_unmapped`](crate::ToplevelHandler::surface_unmapped).
    pub fn unmap(&self) {
        // SAFETY: the handle borrows a live surface; wlroots unmaps it and
        // emits its own unmap signal through the listeners this crate linked.
        unsafe { sys::wlr_surface_unmap(self.raw.as_ptr()) };
    }

    /// This surface's `wlr_layer_surface_v1` role, if it is one.
    ///
    /// `None` for any other role, for a destroyed layer surface, and when this
    /// crate never attached a layer-surface id to the surface. The returned
    /// handle borrows this `Surface` and cannot outlive it.
    #[must_use]
    pub fn as_layer_surface(&self) -> Option<LayerSurface<'_>> {
        // SAFETY: the handle borrows a live surface; the downcast reads its
        // role and returns null rather than a wrong object when it is not a
        // layer surface, or when wlroots has already freed the role.
        unsafe {
            let raw = sys::wlr_layer_surface_v1_try_from_wlr_surface(self.raw.as_ptr());
            if raw.is_null() {
                return None;
            }
            let id = find_id(&raw const (*self.raw.as_ptr()).addons).map(LayerSurfaceId)?;
            Some(LayerSurface::from_raw_with_id(raw, id))
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
                seat: None,
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

    /// A surface with no role sits at the root of its own tree, and the id
    /// resolver hands that same surface back rather than inventing one.
    #[test]
    fn root_id_of_a_roleless_surface_is_its_own_id() {
        let scratch = ScratchSurface::new();
        let surface = unsafe { scratch.surface(SurfaceId(7)) };
        assert_eq!(surface.root_id(), SurfaceId(7));
    }

    /// With no seat cached (a handle built outside a runtime) the touch query
    /// answers `false` without touching wlroots.
    #[test]
    fn accepts_touch_without_a_seat_is_false() {
        let scratch = ScratchSurface::new();
        let surface = unsafe { scratch.surface(SurfaceId(0)) };
        assert!(!surface.accepts_touch());
    }

    /// A nonpositive preferred scale trips wlroots' own assert, so the safe
    /// wrapper refuses it before reaching the call.
    #[test]
    fn nonpositive_preferred_scale_is_refused() {
        let scratch = ScratchSurface::new();
        let surface = unsafe { scratch.surface(SurfaceId(0)) };
        assert_eq!(surface.set_preferred_buffer_scale(0), None);
        assert_eq!(surface.set_preferred_buffer_scale(-1), None);
    }

    /// Two handles from one runtime share a scope lifetime, so `'h` alone does
    /// not stop a token minted on one surface from being offered to another —
    /// and per-surface sequence numbers start at the same values, so replaying
    /// it would reach wlroots' `cached_state_locks > 0` assert and abort. The
    /// identity check refuses it instead.
    #[test]
    fn a_pending_lock_cannot_be_replayed_on_another_surface() {
        let a = ScratchSurface::new();
        let b = ScratchSurface::new();
        let surface_a = unsafe { a.surface(SurfaceId(1)) };
        let surface_b = unsafe { b.surface(SurfaceId(2)) };
        let lock = surface_a.lock_pending();
        assert_eq!(surface_b.unlock_cached(lock), None);
    }
}
