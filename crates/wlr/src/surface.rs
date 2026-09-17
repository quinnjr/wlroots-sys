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
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::geom::{Box2D, FBox, Transform};
use crate::id::{dangling_test_id, find_id, find_surface_id};
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
        SurfaceId(dangling_test_id(0))
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
    /// distinct from every other test id must pass `n >= 1`. The banding lives
    /// in `dangling_test_id`, shared with every
    /// id type that offers one, so the reserved range is one policy rather
    /// than one copy per type.
    pub fn dangling_nth_for_test(n: u64) -> SurfaceId {
        SurfaceId(dangling_test_id(n))
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

/// Outstanding [`PendingLock`]s, for the test-only leak check.
///
/// Incremented by [`Surface::lock_pending`](crate::Surface::lock_pending) and
/// decremented on a real unlock — by [`Surface::unlock_cached`](crate::Surface::unlock_cached),
/// or by [`PendingLock`]'s `Drop` when a lock is dropped without unlocking.
/// A refused unlock hands the token back, so it neither decrements here nor
/// needs to: the caller still owns the lock. Unit tests assert this returns
/// to its entry value; production code never reads it.
#[cfg(test)]
static PENDING_LOCKS_OUTSTANDING: AtomicU64 = AtomicU64::new(0);

/// How many [`PendingLock`]s are currently outstanding (tests only).
///
/// Test hook for the forget-is-fatal contract
/// [`Surface::lock_pending`](crate::Surface::lock_pending) documents: a leaked
/// lock shows up here, since nothing else ever releases one. Test-gated rather
/// than `debug_assertions`-gated, so non-test builds carry no counter at all
/// (and no dead-code warning for the hook nothing outside tests reads).
#[cfg(test)]
fn pending_locks_outstanding() -> u64 {
    PENDING_LOCKS_OUTSTANDING.load(Ordering::Relaxed)
}

/// A held [`Surface::lock_pending`](crate::Surface::lock_pending), released by
/// handing it back to [`Surface::unlock_cached`](crate::Surface::unlock_cached).
///
/// Opaque, and neither `Clone` nor `Copy`. wlroots'
/// `wlr_surface_unlock_cached` aborts when a state is unlocked with no matching
/// lock, so the type system is the first guard: a value only `lock_pending` can
/// mint, consumed on release. The second is the recorded surface identity:
/// `unlock_cached` compares it before the call and hands the token back when it
/// was minted by a different surface, which is otherwise reachable because two
/// handles from one [`Runtime`](crate::Runtime) share a scope lifetime.
///
/// The identity is the issuing surface's address *and* its [`SurfaceId`]. The
/// address alone would alias after free+realloc (ABA): a token minted on a dead
/// surface could name the address of an unrelated live one. The id cannot alias
/// the same way — it comes from the process-wide counter backing every id in
/// this crate, which starts at 1, only increments and never reuses a value, so
/// a new surface at a recycled address carries a different id and the pair
/// still mismatches. Bound: tokens minted on the *same* surface share the pair
/// and differ only by sequence number, which `unlock_cached` deliberately does
/// not order — same-surface discipline is wlroots' own lock accounting, and a
/// double-release of one token is stopped by move semantics (the token is
/// consumed), not by a check here.
///
/// The lifetime ties the token to the scope that produced it. `!Send`/`!Sync`
/// comes from the `_not_send` marker ([`Rc`](std::rc::Rc) is neither): the
/// raw surface pointer alone would not do it, since every field here is
/// otherwise `Send + Sync` and the token must never cross threads — wlroots'
/// lock accounting lives on the event loop that minted it.
///
/// A dropped lock still unlocks (see the `Drop` impl below), but without
/// [`unlock_cached`](Surface::unlock_cached)'s issuing-surface check, so
/// releasing through `unlock_cached` stays the only correct path.
#[derive(Debug)]
#[must_use = "release a pending lock with unlock_cached: dropping it unlocks without the issuing-surface check"]
pub struct PendingLock<'h> {
    seq: u32,
    surface: *mut sys::wlr_surface,
    id: SurfaceId,
    _scope: PhantomData<&'h ()>,
    _not_send: PhantomData<std::rc::Rc<()>>,
}

/// A forgotten [`PendingLock`] would strand its surface — wlroots keeps the
/// pending state cached until every lock is released, with no error anywhere
/// — so dropping one unlocks rather than leaking it.
///
/// This is the backstop, not the path: unlike
/// [`unlock_cached`](Surface::unlock_cached) it cannot refuse a token minted
/// by another surface, it just releases. `unlock_cached` consumes the token
/// with [`mem::forget`](std::mem::forget) after its own unlock precisely so
/// this impl does not run twice for one lock.
impl Drop for PendingLock<'_> {
    fn drop(&mut self) {
        // SAFETY: the token names a lock `lock_pending` took on this exact
        // surface, and the token's scope lifetime keeps it from outliving the
        // handler the issuing handle was built for — the surface is live for
        // the whole scope, so the `cached_state_locks > 0` assert wlroots
        // checks holds here exactly as it does in `unlock_cached`.
        unsafe { sys::wlr_surface_unlock_cached(self.surface, self.seq) };
        #[cfg(test)]
        PENDING_LOCKS_OUTSTANDING.fetch_sub(1, Ordering::Relaxed);
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
    /// Build a handle for a certainly-live surface.
    ///
    /// The single constructor every builder in this module routes through, so
    /// the field list exists exactly once. [`from_raw_with_id`](Self::from_raw_with_id)
    /// checks a nullable pointer then calls this; the infallible call sites —
    /// `backend::with_surface` and [`Runtime::surface`](crate::Runtime::surface),
    /// which hold a table entry that is removed before wlroots frees its
    /// surface — build straight from the entry they already hold as `NonNull`,
    /// so a handler-delivery path never panics here.
    ///
    /// # Safety
    ///
    /// `raw` must be a live `wlr_surface` whose addon set carries `id`, and the
    /// returned handle must not outlive the callback it was created for.
    unsafe fn from_non_null(raw: NonNull<sys::wlr_surface>, id: SurfaceId) -> Surface<'h> {
        Surface {
            raw,
            id,
            tearing_manager: None,
            seat: None,
            _scope: PhantomData,
        }
    }

    /// # Safety
    ///
    /// `raw` must be a live `wlr_surface` whose addon set carries `id`, and the
    /// returned handle must not outlive the callback it was created for.
    ///
    /// The null check aborts deliberately: a null here is a programming error
    /// at an infallible call site, not a miss. Nullable callers must use
    /// [`from_raw_opt`](Self::from_raw_opt) instead.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_surface,
        id: SurfaceId,
    ) -> Surface<'h> {
        // The null check stays here for the nullable-pointer callers (the
        // tests' scratch constructor); the infallible production call sites
        // build from an already-`NonNull` entry via `from_non_null` instead.
        let raw = NonNull::new(raw).expect("wlroots handed us a null surface");
        // SAFETY: this function's own contract, narrowed to non-null.
        unsafe { Surface::from_non_null(raw, id) }
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
        NonNull::new(raw).map(|raw| {
            // SAFETY: this function's own contract, narrowed to non-null.
            unsafe { Surface::from_non_null(raw, id) }
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
    /// so [`Surface::tearing_hint`] on it reports the no-hint answer, and
    /// without a seat, so [`Surface::accepts_touch`] on it always reports
    /// `false` — the same disclosure [`surface_at`](Surface::surface_at)
    /// makes; every other accessor reads the live surface normally.
    ///
    /// A surface the crate never attached an id to is silently skipped rather
    /// than visited with a fabricated one — reachable only for a surface built
    /// outside the announce path, since every announced surface goes through
    /// `install_surface_listeners`.
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

    /// Shared tail of the surface-tree hit-tests: turn a walk's raw hit into a
    /// handle, or `None` for a miss or an untracked surface.
    ///
    /// The three `surface_at_impl`s (here, [`Toplevel`](crate::Toplevel)'s and
    /// [`LayerSurface`](crate::LayerSurface)'s) differ only in which wlroots
    /// walk produces `raw` — each walk takes a different root-pointer type but
    /// returns the same `*mut wlr_surface` — so this one tail serves all of
    /// them and the per-type impls stop re-stating the null check, the id
    /// lookup and the handle construction.
    ///
    /// # Safety
    ///
    /// `raw` must be null or a live `wlr_surface` whose addon set carries a
    /// surface id, and the returned handle must not outlive the tree the walk
    /// ran against.
    pub(crate) unsafe fn finish_surface_at<'s>(
        raw: *mut sys::wlr_surface,
        sub_x: f64,
        sub_y: f64,
    ) -> Option<(Surface<'s>, f64, f64)> {
        if raw.is_null() {
            return None;
        }
        // SAFETY: this function's own contract: non-null means a live surface
        // whose addon set carries a surface id.
        let id = unsafe { find_surface_id(&raw const (*raw).addons).map(SurfaceId)? };
        // SAFETY: same contract; a null `raw` returned above, so this one is
        // live and `from_raw_opt` cannot miss.
        let surface = unsafe { Surface::from_raw_opt(raw, id)? };
        Some((surface, sub_x, sub_y))
    }

    /// Shared walk preamble for the surface-tree hit-tests: run `walk` against
    /// `root` and return the raw hit with the leaf-relative coordinates.
    ///
    /// Generic over the root-pointer type because each wlroots walk takes a
    /// different one (`*mut wlr_surface` here, `*mut wlr_xdg_surface` and
    /// `*mut wlr_layer_surface_v1` on the role handles) while returning the
    /// same `*mut wlr_surface`. The per-type `surface_at_impl`s keep only
    /// their root selection; this preamble and
    /// [`finish_surface_at`](Surface::finish_surface_at)'s tail exist exactly
    /// once.
    ///
    /// # Safety
    ///
    /// `root` must be a live root of the kind `walk` expects, `walk` must be
    /// the matching wlroots hit-test for it (returning null or a live surface
    /// of that same tree), and the returned raw pointer must be consumed
    /// through `finish_surface_at` without outliving the tree the walk ran
    /// against.
    pub(crate) unsafe fn walk_surface_at<R>(
        root: R,
        walk: unsafe extern "C" fn(R, f64, f64, *mut f64, *mut f64) -> *mut sys::wlr_surface,
        sx: f64,
        sy: f64,
    ) -> (*mut sys::wlr_surface, f64, f64) {
        let mut sub_x = 0.0;
        let mut sub_y = 0.0;
        // SAFETY: the caller guarantees `root` is live and `walk` is its
        // matching hit-test; both out-parameters are live locals that outlive
        // the call, and wlroots only reads the coordinates.
        let raw = unsafe { walk(root, sx, sy, &raw mut sub_x, &raw mut sub_y) };
        (raw, sub_x, sub_y)
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
        // SAFETY: the handle borrows a live surface, which is the root this
        // walk expects; the hit is consumed through `finish_surface_at`, which
        // is what that function's contract takes.
        unsafe {
            let (raw, sub_x, sub_y) = Self::walk_surface_at(self.raw.as_ptr(), walk, sx, sy);
            Self::finish_surface_at(raw, sub_x, sub_y)
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
    /// a sequence number or release the same lock twice. It records the issuing
    /// surface's address and id, which is what lets `unlock_cached` hand back
    /// a token offered to the wrong surface instead of reaching that abort.
    ///
    /// `mem::forget`ting the token strands the surface silently — the `Drop`
    /// backstop never runs. Dropping it (an early return past the unlock, a
    /// `drop(lock)`) still releases the lock, but without the
    /// issuing-surface check, so unlock before every exit, including the error
    /// paths:
    ///
    /// ```ignore
    /// let lock = surface.lock_pending();
    /// if let Err(e) = prepare_frame(&surface) {
    ///     // The token is still owned here: hand it back before returning, or
    ///     // the release skips the issuing-surface check.
    ///     let _ = surface.unlock_cached(lock);
    ///     return Err(e);
    /// }
    /// commit_frame(&surface);
    /// surface.unlock_cached(lock).expect("unlocked on its own surface");
    /// ```
    pub fn lock_pending(&self) -> PendingLock<'h> {
        // SAFETY: the handle borrows a live surface; the call only touches its
        // own state.
        #[cfg(test)]
        PENDING_LOCKS_OUTSTANDING.fetch_add(1, Ordering::Relaxed);
        PendingLock {
            seq: unsafe { sys::wlr_surface_lock_pending(self.raw.as_ptr()) },
            surface: self.raw.as_ptr(),
            id: self.id,
            _scope: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// Release one [`lock_pending`](Surface::lock_pending) on this surface.
    ///
    /// The state is not guaranteed to commit immediately, because another lock
    /// may still be outstanding. `Err(lock)` when `lock` was minted by a
    /// different surface: two handles can share a scope lifetime, so the check
    /// is what keeps a cross-surface replay from reaching wlroots' assert —
    /// the token embeds the issuing surface's address and id, and this method
    /// hands back any token naming another. The error hands ownership back
    /// rather than consuming the token, so a refused unlock never strands the
    /// issuing surface's lock count: the caller still owns the lock and must
    /// release it on the surface that minted it.
    pub fn unlock_cached(&self, lock: PendingLock<'h>) -> Result<(), PendingLock<'h>> {
        if lock.surface != self.raw.as_ptr() || lock.id != self.id {
            return Err(lock);
        }
        // SAFETY: `lock` names a lock this exact surface took (the identity
        // check above), so wlroots' `cached_state_locks > 0` assert holds; the
        // handle borrows a live surface.
        unsafe { sys::wlr_surface_unlock_cached(self.raw.as_ptr(), lock.seq) };
        #[cfg(test)]
        PENDING_LOCKS_OUTSTANDING.fetch_sub(1, Ordering::Relaxed);
        // The counter and the wlroots lock are both released: the token must
        // not run them again. `Drop` would unlock a second time (and
        // double-decrement the counter), aborting on wlroots'
        // `cached_state_locks > 0` assert — so it is forgotten, not dropped.
        std::mem::forget(lock);
        Ok(())
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
            // SAFETY: `surface` was null-checked above and is live per
            // `for_each_surface_with`'s contract; `id` was just read from its
            // own addon set.
            let handle = Surface::from_non_null(NonNull::new_unchecked(surface), id);
            f(&handle, sx, sy);
        }
    }

    call(Some(visit::<F>), std::ptr::from_mut(f).cast());
}

#[cfg(test)]
mod tests {
    use super::{SurfaceId, SurfaceRole};
    use crate::test_support::ScratchSurface;

    /// `PendingLock` must never cross threads: wlroots' lock accounting lives
    /// on the event loop that minted the token. The raw surface pointer alone
    /// does not give that — every field would otherwise be `Send + Sync` —
    /// so the `_not_send` marker carries it; this pins the marker's effect.
    #[test]
    fn a_pending_lock_neither_sends_nor_syncs() {
        use static_assertions::assert_not_impl_any;

        assert_not_impl_any!(super::PendingLock<'static>: Send, Sync);
    }

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

    /// `SurfaceRole::from_raw` decodes the protocol's own discriminants and
    /// answers `None` — never a panic — for anything else: the value is set
    /// by wlroots from a client's request, and a compositor must not abort
    /// on a value this build does not know.
    #[test]
    fn surface_role_from_raw_maps_unknown_discriminants_to_none() {
        use crate::sys;
        assert_eq!(
            SurfaceRole::from_raw(sys::wlr_xdg_surface_role(1)),
            SurfaceRole::Toplevel
        );
        assert_eq!(
            SurfaceRole::from_raw(sys::wlr_xdg_surface_role(2)),
            SurfaceRole::Popup
        );
        for raw in [0, 3, 42, u32::MAX] {
            assert_eq!(
                SurfaceRole::from_raw(sys::wlr_xdg_surface_role(raw)),
                SurfaceRole::None,
                "out-of-range discriminant {raw} must read as None"
            );
        }
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
    /// identity check hands the token back instead, unconsumed, so the issuing
    /// surface's lock count never strands: the refused token stays usable where
    /// it was minted.
    #[test]
    fn a_pending_lock_cannot_be_replayed_on_another_surface() {
        let a = ScratchSurface::new();
        let b = ScratchSurface::new();
        let surface_a = unsafe { a.surface(SurfaceId(1)) };
        let surface_b = unsafe { b.surface(SurfaceId(2)) };
        let outstanding_before = super::pending_locks_outstanding();
        let lock = surface_a.lock_pending();
        let lock = match surface_b.unlock_cached(lock) {
            Err(lock) => lock,
            Ok(()) => panic!("a cross-surface unlock must be refused"),
        };
        // The refused token survived: it still releases its own surface, and
        // the lock count returns to its entry value.
        assert!(surface_a.unlock_cached(lock).is_ok());
        assert_eq!(
            super::pending_locks_outstanding(),
            outstanding_before,
            "a refused unlock neither leaks nor double-releases"
        );
    }

    /// Dropping a lock without unlocking still releases it: the `Drop`
    /// backstop unlocks rather than stranding the surface, so the test-only
    /// counter returns to its entry value with no `unlock_cached` call.
    #[test]
    fn dropping_a_pending_lock_without_unlocking_releases_it() {
        let scratch = ScratchSurface::new();
        let surface = unsafe { scratch.surface(SurfaceId(1)) };
        let outstanding_before = super::pending_locks_outstanding();
        {
            let _lock = surface.lock_pending();
            assert_eq!(
                super::pending_locks_outstanding(),
                outstanding_before + 1,
                "the lock is outstanding while held"
            );
            // No `unlock_cached`: the end of this block drops the token.
        }
        assert_eq!(
            super::pending_locks_outstanding(),
            outstanding_before,
            "dropping the token released the lock"
        );
    }

    /// The success path unlocks exactly once: `unlock_cached` consumes the
    /// token with `mem::forget` after its own unlock, so `Drop` must not run
    /// a second unlock (and a second counter decrement) for the same lock.
    #[test]
    fn unlocking_a_pending_lock_balances_the_counter_exactly_once() {
        let scratch = ScratchSurface::new();
        let surface = unsafe { scratch.surface(SurfaceId(1)) };
        let outstanding_before = super::pending_locks_outstanding();
        let lock = surface.lock_pending();
        assert!(surface.unlock_cached(lock).is_ok());
        assert_eq!(
            super::pending_locks_outstanding(),
            outstanding_before,
            "a released lock decrements exactly once — no double-unlock from Drop"
        );
    }

    /// One half of the ABA pair is not enough: a token offered to a handle at
    /// the same address but carrying a different id is refused, and the token
    /// still unlocks the surface that minted it.
    #[test]
    fn unlock_cached_refuses_a_same_address_token_with_a_different_id() {
        let scratch = ScratchSurface::new();
        let minter = unsafe { scratch.surface(SurfaceId(1)) };
        let impostor = unsafe { scratch.surface(SurfaceId(2)) };
        let outstanding_before = super::pending_locks_outstanding();
        let lock = minter.lock_pending();
        let lock = match impostor.unlock_cached(lock) {
            Err(lock) => lock,
            Ok(()) => panic!("a same-address/different-id unlock must be refused"),
        };
        assert!(minter.unlock_cached(lock).is_ok());
        assert_eq!(
            super::pending_locks_outstanding(),
            outstanding_before,
            "the refused token still released exactly once, on its minter"
        );
    }

    /// The other half: a token offered to a different surface that happens to
    /// carry the same id value is refused too, and still unlocks its minter.
    #[test]
    fn unlock_cached_refuses_a_same_id_token_from_a_different_address() {
        let a = ScratchSurface::new();
        let b = ScratchSurface::new();
        let surface_a = unsafe { a.surface(SurfaceId(1)) };
        let surface_b = unsafe { b.surface(SurfaceId(1)) };
        let outstanding_before = super::pending_locks_outstanding();
        let lock = surface_a.lock_pending();
        let lock = match surface_b.unlock_cached(lock) {
            Err(lock) => lock,
            Ok(()) => panic!("a different-address/same-id unlock must be refused"),
        };
        assert!(surface_a.unlock_cached(lock).is_ok());
        assert_eq!(
            super::pending_locks_outstanding(),
            outstanding_before,
            "the refused token still released exactly once, on its minter"
        );
    }

    /// The null check in `from_raw_with_id` aborts deliberately: a null at an
    /// infallible call site is a programming error, not a miss — nullable
    /// callers must use `from_raw_opt`.
    #[test]
    #[should_panic(expected = "wlroots handed us a null surface")]
    fn from_raw_with_id_aborts_on_null_deliberately() {
        // SAFETY: none — null is the whole point; the abort is the contract.
        unsafe {
            super::Surface::from_raw_with_id(std::ptr::null_mut(), SurfaceId(0));
        }
    }

    /// `from_raw_opt` is the nullable sibling: null reads `None` rather
    /// than aborting, so nullable callers (hit-tests, `extern "C"` frames)
    /// never abort through C.
    #[test]
    fn from_raw_opt_returns_none_on_null() {
        // SAFETY: none — null is the whole point; `None` is the contract.
        let got = unsafe { super::Surface::from_raw_opt(std::ptr::null_mut(), SurfaceId(0)) };
        assert!(got.is_none(), "null must read None, not abort or fabricate");
    }

    /// `role()` on a surface with no xdg-surface role reads `None` rather
    /// than faulting: wlroots' `try_from` answers null for a roleless surface
    /// (a zeroed scratch has a null role), which is the ordinary "no role"
    /// state, not a lookup failure.
    #[test]
    fn role_of_a_roleless_surface_is_none() {
        let scratch = ScratchSurface::new();
        let surface = unsafe { scratch.surface(SurfaceId(0)) };
        assert_eq!(surface.role(), SurfaceRole::None);
    }

    /// Test ids stay inside the reserved band at the top of the id space no
    /// matter how large `n` is: the modulo fold keeps even `u64::MAX` above
    /// the band floor, so no test id can collide with a real surface's.
    ///
    /// `n = 2^32` aliases `dangling_for_test` by that same modulo rule
    /// (`2^32 ≡ 0`), exactly as `n = 0` does — it is in-band but not
    /// distinct, so callers wanting distinctness must pass `n >= 1` with
    /// `n % 2^32 != 0`.
    #[test]
    fn surface_id_test_ids_stay_in_the_reserved_band_for_large_n() {
        let floor = u64::MAX - ((1u64 << 32) - 1);
        for n in [u64::MAX, u64::MAX - 1] {
            let id = SurfaceId::dangling_nth_for_test(n);
            assert!(
                id.0 >= floor,
                "dangling_nth_for_test({n}) = {} left the reserved band",
                id.0
            );
            assert_ne!(
                id,
                SurfaceId::dangling_for_test(),
                "dangling_nth_for_test({n}) must stay distinct from dangling_for_test"
            );
        }
        let wrapped = SurfaceId::dangling_nth_for_test(1u64 << 32);
        assert!(
            wrapped.0 >= floor,
            "dangling_nth_for_test(2^32) = {} left the reserved band",
            wrapped.0
        );
        assert_eq!(
            wrapped,
            SurfaceId::dangling_for_test(),
            "2^32 folds to 0, so it aliases dangling_for_test like n = 0 does"
        );
    }

    /// Initialise the two child-list heads the C tree walks read, on a scratch
    /// surface that is otherwise zeroed. Without this the walks would chase
    /// null list links; with it an empty tree is exactly what the walk sees.
    ///
    /// # Safety
    ///
    /// `raw` must be a live, exclusively-owned scratch allocation.
    unsafe fn init_walk_lists(raw: *mut crate::sys::wlr_surface) {
        // SAFETY: the caller guarantees exclusive ownership; each initialiser
        // writes only the `wl_list` head it owns, which then points at itself.
        unsafe {
            crate::sys::wayland_sys::server::wl_list_init(
                &raw mut (*raw).current.subsurfaces_below,
            );
            crate::sys::wayland_sys::server::wl_list_init(
                &raw mut (*raw).current.subsurfaces_above,
            );
        }
    }

    /// The tree walk visits a tracked root with its attached id at `(0, 0)`:
    /// the `visit` thunk resolves the id from the surface's own addon set,
    /// not from the handle the walk started from.
    #[test]
    fn for_each_surface_visits_a_tracked_root_with_its_own_id() {
        let _serialised = crate::id::id_test_lock();
        let scratch = ScratchSurface::new();
        // SAFETY: `scratch` is exclusively owned here; the attach finishes
        // with the set when `scratch` drops, while this lock is still held.
        let attached = unsafe {
            init_walk_lists(scratch.raw);
            crate::id::attach_surface_id(&raw mut (*scratch.raw).addons)
                .expect("first attach succeeds")
        };
        let surface = unsafe { scratch.surface(SurfaceId(0)) };
        let mut visited = Vec::new();
        surface.for_each_surface(|leaf, x, y| visited.push((leaf.id(), x, y)));
        assert_eq!(
            visited,
            vec![(SurfaceId(attached), 0, 0)],
            "the walk yields the root once, with the id from its addon set"
        );
    }

    /// A surface the crate never attached an id to is silently skipped: the
    /// closure never runs, and in particular no handle is fabricated for it.
    #[test]
    fn for_each_surface_skips_a_surface_without_an_id() {
        let scratch = ScratchSurface::new();
        // SAFETY: `scratch` is exclusively owned; no addon is attached, so no
        // destroy hook runs at drop.
        unsafe {
            init_walk_lists(scratch.raw);
        }
        let surface = unsafe { scratch.surface(SurfaceId(0)) };
        let mut visits = 0;
        surface.for_each_surface(|_, _, _| visits += 1);
        assert_eq!(visits, 0, "an id-less surface must be skipped, not visited");
    }

    /// The `visit` thunk refuses null surface/data pointers without running
    /// the closure: a null here would be a dereference in an `extern "C"`
    /// frame, so it is refused rather than dereferenced — and refusal must
    /// not panic either, since a panic out of `extern "C"` aborts. Each
    /// half is tripped separately.
    #[test]
    fn visit_refuses_null_pointers_without_running_the_closure() {
        use std::ffi::c_void;
        let scratch = crate::test_support::ScratchSurface::new();
        let mut visits = 0;
        let mut f = |_: &super::Surface<'_>, _: i32, _: i32| visits += 1;
        // SAFETY: the stubs invoke the handed iterator synchronously with
        // the pointers named; `scratch` outlives both calls, and neither
        // call dereferences (that is the point under test).
        unsafe {
            super::for_each_surface_with(&mut f, |iterate, data| {
                iterate.expect("iterator fn")(std::ptr::null_mut(), 0, 0, data);
            });
            super::for_each_surface_with(&mut f, |iterate, _| {
                iterate.expect("iterator fn")(scratch.raw, 0, 0, std::ptr::null_mut::<c_void>());
            });
        }
        assert_eq!(visits, 0, "null surface/data must skip the closure");
        drop(scratch);
    }

    /// `surface_at` on a childless scratch surface misses through the real
    /// wlroots walk (zeroed dimensions reject every non-negative point before
    /// any region is touched), and the shared tail reports a live but
    /// id-less raw pointer as a miss rather than fabricating a handle — the
    /// no-invention rule every `surface_at` family in the crate shares.
    #[test]
    fn surface_at_on_an_untracked_leaf_misses_rather_than_fabricating() {
        let scratch = ScratchSurface::new();
        // SAFETY: `scratch` is exclusively owned; no addon is attached, so no
        // destroy hook runs at drop.
        unsafe {
            init_walk_lists(scratch.raw);
        }
        let surface = unsafe { scratch.surface(SurfaceId(0)) };
        assert!(
            surface.surface_at(1.0, 1.0).is_none(),
            "the real walk misses a zeroed childless surface"
        );
        // SAFETY: `scratch` is live and id-less, which is exactly the miss
        // case under test; the returned handle (none) outlives nothing.
        let tail = unsafe { super::Surface::finish_surface_at(scratch.raw, 0.0, 0.0) };
        assert!(
            tail.is_none(),
            "an untracked leaf must miss, not resolve to an invented id"
        );
    }
}
