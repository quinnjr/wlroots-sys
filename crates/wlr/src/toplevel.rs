//! Borrow-scoped toplevel handles and their stable ids.
//!
//! Same shape as [`Output`](crate::Output), for the same reason: a
//! `wlr_xdg_toplevel` is freed whenever its client says so, so a handle that
//! escapes the handler it was passed to is a use-after-free. The lifetime and
//! the private constructor make that a compile error.
//!
//! The id is attached with `wlr_addon` to the toplevel's **surface**, not to
//! the toplevel itself: `wlr_xdg_toplevel` has no addon set, `wlr_surface`
//! does, and the two die together (wlroots destroys the toplevel role object
//! with the surface that carries it). So wlroots runs the id's destructor at
//! exactly the right moment and nothing has to be swept.

use std::ffi::CStr;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::id::{find_id, find_surface_id};
use crate::{Surface, sys};

/// Identifies a toplevel for as long as the consumer chooses to remember it.
///
/// Storable, comparable and hashable — unlike a handle. Ids are never reused
/// within a process, and an id held past its toplevel's destruction resolves
/// to nothing rather than to another window.
///
/// Deliberately no `PartialOrd`/`Ord`: an opaque id's ordering would promise
/// creation-order semantics nobody asked for, and this API is frozen within
/// the wlroots minor, so a derive added here could not be withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToplevelId(pub(crate) u64);

impl ToplevelId {
    /// An id no live toplevel can have, for testing the "unknown id" path.
    ///
    /// Public because "every by-id operation reports a miss rather than
    /// dereferencing" is a promise to consumers, and a promise nobody can
    /// write a test for is not one. Not the only way to obtain a `ToplevelId`
    /// outside a handler any more —
    /// [`dangling_nth_for_test`](Self::dangling_nth_for_test) is another —
    /// but this one and that one are the *only* ways, the field being
    /// private, so hiding either from the docs would leave the promise
    /// untestable in practice while still freezing the function.
    ///
    /// That every by-id operation misses on this value is part of the frozen
    /// contract, not an implementation accident: ids come from a process-wide
    /// counter that starts at 1, only ever increments, and never reuses a
    /// value, so `u64::MAX` cannot be handed to a real toplevel.
    ///
    /// Not for production code. An id from a real toplevel is the one
    /// [`Toplevel::id`] returns, and it stops resolving once the
    /// [`Backend::run_all`](crate::Backend::run_all) call that announced it
    /// has returned — at which point it behaves exactly like this one.
    pub fn dangling_for_test() -> ToplevelId {
        ToplevelId(u64::MAX)
    }

    /// A distinct id no live toplevel can have, for testing.
    ///
    /// Ids issued to real toplevels come from a counter that starts at 1 and
    /// increments, so no process will reach the top of the range; `n` picks
    /// one of that reserved band. Public for the same reason
    /// `dangling_for_test` is: the "unknown id is a miss, not a crash"
    /// promise needs to be testable by consumers, and a compositor's own
    /// tests need to drive their handler logic without a client -- and with
    /// more than one dangling id in play at once, which a single fixed value
    /// cannot give them.
    ///
    /// Callers wanting an id that also never collides with
    /// [`dangling_for_test`](Self::dangling_for_test)'s must pass `n >= 1`:
    /// `dangling_nth_for_test(0)` is `dangling_for_test()` itself.
    ///
    /// `n` is folded into a fixed 2^32-wide band immediately below
    /// `u64::MAX` (`n % 2^32`) rather than subtracted from `u64::MAX`
    /// unclamped. Every id this crate ever issues comes from `next_id`,
    /// a single process-wide `u64` counter shared by every id type in the
    /// crate — so a caller passing a very large `n` (this crate's own tests
    /// probe `n = u64::MAX`) must still land on a value the counter cannot
    /// reach in this process's lifetime, or the "no live toplevel can have
    /// this id" guarantee above would be false for that call. The band is
    /// still 2^32 ids wide, which is far more than any test needs, and
    /// `n = 0` still aliases [`dangling_for_test`](Self::dangling_for_test)
    /// and `n` in `1..=8` is unaffected (`n % 2^32 == n` for those).
    pub fn dangling_nth_for_test(n: u64) -> ToplevelId {
        ToplevelId(u64::MAX - (n % (1u64 << 32)))
    }
}

/// An xdg toplevel, borrowed for the duration of a handler call.
pub struct Toplevel<'h> {
    raw: NonNull<sys::wlr_xdg_toplevel>,
    id: ToplevelId,
    _scope: PhantomData<&'h ()>,
}

/// Hand-written rather than derived, for the same reason
/// [`KeyEvent`](crate::KeyEvent)'s is: the `PhantomData` scope marker has no
/// value to print, and a raw pointer printed by a derive is neither useful
/// nor stable across runs. Named fields only: `id`, `title`, `app_id`.
impl std::fmt::Debug for Toplevel<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toplevel")
            .field("id", &self.id)
            .field("title", &self.title())
            .field("app_id", &self.app_id())
            .finish()
    }
}

impl<'h> Toplevel<'h> {
    /// # Safety
    ///
    /// `raw` must be a live `wlr_xdg_toplevel` whose surface carries the id
    /// addon that produced `id`, and the returned handle must not outlive the
    /// callback it was created for.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_xdg_toplevel,
        id: ToplevelId,
    ) -> Toplevel<'h> {
        Toplevel {
            raw: NonNull::new(raw).expect("wlroots handed us a null toplevel"),
            id,
            _scope: PhantomData,
        }
    }

    /// This toplevel's stable identity, safe to store beyond the handler.
    pub fn id(&self) -> ToplevelId {
        self.id
    }

    /// The raw toplevel, for the in-crate callers that pass it to wlroots.
    pub(crate) fn as_ptr(&self) -> *mut sys::wlr_xdg_toplevel {
        self.raw.as_ptr()
    }

    /// The client's `xdg_toplevel.set_title`, if it has sent one.
    pub fn title(&self) -> Option<String> {
        // SAFETY: the handle's lifetime guarantees the toplevel is live;
        // wlroots leaves `title` null until the client sets one.
        unsafe { cstr_field((*self.raw.as_ptr()).title) }
    }

    /// The client's `xdg_toplevel.set_app_id`, if it has sent one.
    pub fn app_id(&self) -> Option<String> {
        // SAFETY: as for `title`.
        unsafe { cstr_field((*self.raw.as_ptr()).app_id) }
    }

    /// The client's current surface size, in content (client-owned) pixels —
    /// the counterpart to the size
    /// [`Runtime::set_toplevel_size`](crate::Runtime::set_toplevel_size)
    /// stages: that call sets what the *next* configure asks for, this
    /// reads what the client's most recent commit actually produced.
    ///
    /// `(0, 0)` before the client's first commit, since that is what
    /// wlroots' own `wlr_surface_state` starts zeroed at and this reads it
    /// directly rather than tracking a separate "has it mapped yet" flag of
    /// its own.
    ///
    /// # Panics
    ///
    /// Never: the handle's lifetime guarantees the toplevel is live, and a
    /// live `wlr_xdg_toplevel` always has a non-null `base` (its owning
    /// `wlr_xdg_surface`) and a non-null `base->surface` — both are set once
    /// at role-object creation, before this crate ever hands a `Toplevel` to
    /// a handler, and neither is ever cleared while the toplevel itself is
    /// still alive.
    pub fn current_size(&self) -> (i32, i32) {
        // SAFETY: as this method's own `# Panics` section argues: the
        // handle's lifetime guarantees the toplevel is live, so `base` and
        // `base->surface` are non-null, and `current` is a plain embedded
        // struct (not a pointer) that is always initialised once the
        // surface exists.
        unsafe {
            let base = (*self.raw.as_ptr()).base;
            let surface = (*base).surface;
            let current = &(*surface).current;
            (current.width, current.height)
        }
    }

    /// The pid of the client that owns this toplevel.
    ///
    /// `None` when the resource has no client, which happens only for an
    /// inert resource — a toplevel whose client disconnected between wlroots
    /// queueing an event and this crate delivering it.
    pub fn pid(&self) -> Option<u32> {
        use sys::wayland_sys::ffi_dispatch;
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: the handle's lifetime guarantees the toplevel is live, so
        // its `resource` is a live `wl_resource`; both libwayland calls are
        // reads, and the out-parameters are stack locals of the right types.
        unsafe {
            let resource = (*self.raw.as_ptr()).resource;
            if resource.is_null() {
                return None;
            }
            let client = ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_resource_get_client,
                resource
            );
            if client.is_null() {
                return None;
            }
            // The three out-parameters are `wayland-sys`' own types, not
            // bindgen's look-alikes: `wl_client_get_credentials` is
            // libwayland's, so the identity that has to match is the one the
            // `ffi_dispatch!` function pointer was declared with. bindgen's
            // `sys::pid_t` also only exists when some *gated* wlroots header
            // happens to pull in `<sys/types.h>`, which made this line fail to
            // compile with `--no-default-features`.
            let mut pid: sys::wayland_sys::pid_t = 0;
            let mut uid: sys::wayland_sys::uid_t = 0;
            let mut gid: sys::wayland_sys::gid_t = 0;
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_client_get_credentials,
                client,
                &raw mut pid,
                &raw mut uid,
                &raw mut gid
            );
            u32::try_from(pid).ok()
        }
    }

    /// Downgrade a generic [`Surface`] to its toplevel role, if it has one.
    ///
    /// The counterpart of `wlr_xdg_toplevel_try_from_wlr_surface`. `None` when
    /// the surface is not an `xdg_toplevel`, when its xdg/toplevel role object
    /// has been destroyed, or — defensively — when the surface carries no
    /// toplevel id, which cannot happen for a surface this crate announced but
    /// is handled rather than asserted because a consumer may hand in any
    /// surface.
    ///
    /// The returned handle borrows `surface`, so it cannot outlive it; that is
    /// the same escape-proof rule every role handle follows.
    #[must_use]
    pub fn from_surface(surface: &'h Surface<'_>) -> Option<Toplevel<'h>> {
        // SAFETY: `surface`'s handle borrows a live surface for its lifetime,
        // so its addon set is initialised and the `try_from` is a read.
        unsafe {
            let raw = sys::wlr_xdg_toplevel_try_from_wlr_surface(surface.as_ptr());
            if raw.is_null() {
                return None;
            }
            let id = find_id(&raw const (*surface.as_ptr()).addons).map(ToplevelId)?;
            Some(Self::from_raw_with_id(raw, id))
        }
    }

    /// The toplevel's **current** committed state, copied out.
    ///
    /// Reads `(*toplevel).current`; see [`ToplevelState`] for what each field
    /// means and why the snapshot is a copy rather than a borrow.
    #[must_use]
    pub fn state(&self) -> ToplevelState {
        // SAFETY: the handle's lifetime guarantees the toplevel is live;
        // `current` is a plain embedded `wlr_xdg_toplevel_state`.
        unsafe { ToplevelState::from_c(&(*self.raw.as_ptr()).current) }
    }

    /// The state the client last **requested**, copied out.
    ///
    /// Reads `(*toplevel).requested`; see [`ToplevelRequested`].
    #[must_use]
    pub fn requested(&self) -> ToplevelRequested {
        // SAFETY: as for `state`; `requested` is a plain embedded struct whose
        // booleans `from_c` reads and whose pointers it deliberately does not.
        unsafe { ToplevelRequested::from_c(&(*self.raw.as_ptr()).requested) }
    }

    /// The window-manager capabilities most recently configured for this
    /// toplevel, as a bitmask.
    ///
    /// Reads `(*toplevel).scheduled.wm_capabilities` — the capabilities
    /// wlroots will put in the next configure, which for a toplevel already
    /// mapped are what the client was last told. The client's own request is
    /// not retained by xdg-shell; a compositor decides, and the answer travels
    /// in [`Runtime::set_toplevel_wm_capabilities`](crate::Runtime).
    #[must_use]
    pub fn wm_capabilities(&self) -> WmCapabilities {
        // SAFETY: the handle's lifetime guarantees the toplevel is live;
        // `scheduled` is a plain embedded `wlr_xdg_toplevel_configure`.
        let scheduled: &sys::wlr_xdg_toplevel_configure =
            unsafe { &(*self.raw.as_ptr()).scheduled };
        WmCapabilities::from_raw(scheduled.wm_capabilities)
    }

    /// `wlr_xdg_surface_ping` — ask the client to prove it is still alive.
    ///
    /// A no-op if wlroots later finds the client unresponsive: it emits
    /// `ping_timeout` on the xdg-surface, which this crate exposes through its
    /// own destroy/announce lifecycle. The base is guaranteed non-null for a
    /// live toplevel, so there is nothing to check and no result to report.
    pub fn ping(&self) {
        // SAFETY: the handle's lifetime guarantees the toplevel is live, and a
        // live one always has a non-null `base`; wlroots only sends a ping.
        unsafe { sys::wlr_xdg_surface_ping((*self.raw.as_ptr()).base) };
    }

    /// Call `f` for every surface in this toplevel's **entire** xdg tree —
    /// its own surface, its sub-surfaces and any popups — root first.
    ///
    /// This is the xdg-tree sibling of [`Surface::for_each_surface`]: it wraps
    /// `wlr_xdg_surface_for_each_surface`, so the walk descends through
    /// popups as well as sub-surfaces. See [`Surface::for_each_surface`] for
    /// the closure/handle/tearing-manager rules, which apply verbatim.
    pub fn for_each_surface(&self, mut f: impl FnMut(&Surface<'_>, i32, i32)) {
        // SAFETY: the handle's lifetime guarantees the toplevel is live, so
        // its `base` is a live `wlr_xdg_surface`; the helper runs the walk
        // synchronously and `f` does not outlive the call.
        unsafe {
            let base = (*self.raw.as_ptr()).base;
            crate::surface::for_each_surface_with(&mut f, |iterate, data| {
                sys::wlr_xdg_surface_for_each_surface(base, iterate, data);
            });
        }
    }

    /// Call `f` for every surface in this toplevel's **popup** tree only,
    /// root first.
    ///
    /// The `wlr_xdg_surface_for_each_popup_surface` sibling of
    /// [`for_each_surface`](Self::for_each_surface), for a compositor that
    /// wants to place or render only the popup surfaces. Same closure rules.
    pub fn for_each_popup_surface(&self, mut f: impl FnMut(&Surface<'_>, i32, i32)) {
        // SAFETY: as for `for_each_surface`.
        unsafe {
            let base = (*self.raw.as_ptr()).base;
            crate::surface::for_each_surface_with(&mut f, |iterate, data| {
                sys::wlr_xdg_surface_for_each_popup_surface(base, iterate, data);
            });
        }
    }

    /// Hit-test this toplevel's tree at a point in its own surface-local
    /// coordinates.
    ///
    /// Returns the struck leaf surface, its id, and the point in that leaf's
    /// coordinates; `None` for a miss. This wraps
    /// `wlr_xdg_surface_surface_at` — the walk includes the toplevel's
    /// sub-surfaces.
    #[must_use]
    pub fn surface_at(&self, sx: f64, sy: f64) -> Option<(crate::Surface<'_>, f64, f64)> {
        self.surface_at_impl(sys::wlr_xdg_surface_surface_at, sx, sy)
    }

    /// As [`surface_at`](Self::surface_at), but restricted to the toplevel's
    /// **popup** tree; wraps `wlr_xdg_surface_popup_surface_at`.
    #[must_use]
    pub fn popup_surface_at(&self, sx: f64, sy: f64) -> Option<(crate::Surface<'_>, f64, f64)> {
        self.surface_at_impl(sys::wlr_xdg_surface_popup_surface_at, sx, sy)
    }

    /// Shared body of the two hit-tests, which differ only in which wlroots
    /// walk they call.
    fn surface_at_impl(
        &self,
        walk: unsafe extern "C" fn(
            *mut sys::wlr_xdg_surface,
            f64,
            f64,
            *mut f64,
            *mut f64,
        ) -> *mut sys::wlr_surface,
        sx: f64,
        sy: f64,
    ) -> Option<(crate::Surface<'_>, f64, f64)> {
        let mut sub_x = 0.0;
        let mut sub_y = 0.0;
        // SAFETY: the handle's lifetime guarantees the toplevel is live, so
        // `base` is live; both out-parameters are live locals that outlive the
        // call, and wlroots only reads the coordinates.
        unsafe {
            let base = (*self.raw.as_ptr()).base;
            let raw = walk(base, sx, sy, &raw mut sub_x, &raw mut sub_y);
            if raw.is_null() {
                return None;
            }
            let id = find_surface_id(&raw const (*raw).addons).map(crate::SurfaceId)?;
            let surface = crate::Surface::from_raw_opt(raw, id)?;
            Some((surface, sub_x, sub_y))
        }
    }
}
///
/// A plain `bool` per edge rather than a bitflags type: xdg-shell's
/// `xdg_toplevel_resize_edge` is a small, closed, protocol-frozen set (a
/// corner drag sets two adjacent edges; nothing else is representable), and
/// four named fields make an edge check at the call site
/// (`edges.left`) read straight through, with no bit constant to look up.
///
/// `Default` is every field `false` — no edge — which is also what
/// [`is_empty`](Edges::is_empty) reports on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Edges {
    /// The top edge is being dragged.
    pub top: bool,
    /// The bottom edge is being dragged.
    pub bottom: bool,
    /// The left edge is being dragged.
    pub left: bool,
    /// The right edge is being dragged.
    pub right: bool,
}

impl Edges {
    /// True if no edge is set.
    pub fn is_empty(self) -> bool {
        !(self.top || self.bottom || self.left || self.right)
    }

    /// Decode xdg-shell's `xdg_toplevel_resize_edge` bitmask, as carried by
    /// `wlr_xdg_toplevel_resize_event::edges`: `top` = 1, `bottom` = 2,
    /// `left` = 4, `right` = 8, a corner ORing the two adjacent bits
    /// together. Unknown bits are ignored rather than rejected — a future
    /// protocol extension setting one is not this crate's concern, and
    /// there is nowhere to report it from an event handler that returns
    /// nothing.
    pub(crate) fn from_xdg(bits: u32) -> Edges {
        let (top, bottom, left, right) = decode_edge_bits(bits);
        Edges {
            top,
            bottom,
            left,
            right,
        }
    }

    /// Encode these edges back into wlroots' `wlr_edges` / xdg resize-edge
    /// bitmask — the exact inverse of [`from_xdg`](Edges::from_xdg), and used
    /// by the `set_tiled`/`set_constrained` setters, which are the two wlroots
    /// calls that take the mask rather than the four booleans.
    pub(crate) fn to_xdg(self) -> u32 {
        (self.top as u32)
            | ((self.bottom as u32) << 1)
            | ((self.left as u32) << 2)
            | ((self.right as u32) << 3)
    }
}

/// The window-manager capabilities a compositor advertises to a toplevel.
///
/// A bitmask of `enum wlr_xdg_toplevel_wm_capabilities`, hand-rolled rather
/// than a `bitflags` dependency following
/// [`ConstraintAdjustment`](crate::ConstraintAdjustment): the four bits are
/// the whole domain and are pinned against the generated constants by this
/// module's own tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WmCapabilities(u32);

impl WmCapabilities {
    /// No capabilities advertised; the protocol's initial value, and
    /// [`Default`].
    pub const NONE: WmCapabilities = WmCapabilities(0);
    /// The compositor can show a client window menu.
    pub const WINDOW_MENU: WmCapabilities = WmCapabilities(1);
    /// The compositor can maximize.
    pub const MAXIMIZE: WmCapabilities = WmCapabilities(2);
    /// The compositor can fullscreen.
    pub const FULLSCREEN: WmCapabilities = WmCapabilities(4);
    /// The compositor can minimize.
    pub const MINIMIZE: WmCapabilities = WmCapabilities(8);

    /// Whether **every** bit of `other` is advertised here — the semantics of
    /// [`ConstraintAdjustment::contains`](crate::ConstraintAdjustment::contains),
    /// for the same reason it is not "any".
    #[must_use]
    pub fn contains(self, other: WmCapabilities) -> bool {
        self.0 & other.0 == other.0
    }

    /// The raw mask, as the protocol numbers it.
    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Build from the raw `enum wlr_xdg_toplevel_wm_capabilities` value.
    ///
    /// Unknown bits are kept, not dropped: the mask is handed straight back to
    /// wlroots by [`Runtime::set_toplevel_wm_capabilities`](crate::Runtime),
    /// which is the code that interprets it, so silently clearing a bit would
    /// change the caller's request rather than merely fail to describe it.
    pub(crate) fn from_raw(raw: u32) -> WmCapabilities {
        WmCapabilities(raw)
    }
}

impl std::ops::BitOr for WmCapabilities {
    type Output = WmCapabilities;

    fn bitor(self, rhs: WmCapabilities) -> WmCapabilities {
        WmCapabilities(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for WmCapabilities {
    fn bitor_assign(&mut self, rhs: WmCapabilities) {
        self.0 |= rhs.0;
    }
}

/// A snapshot of a toplevel's **current** state — what wlroots most recently
/// committed, read from `wlr_xdg_toplevel.current`.
///
/// Copied out rather than borrowed, for the same reason
/// [`PositionerRules`](crate::PositionerRules) is: the caller will re-enter
/// wlroots, which can destroy the toplevel, and a view tied to the handle's
/// lifetime would be a use-after-free the borrow checker could not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct ToplevelState {
    /// The compositor has maximized this toplevel.
    pub maximized: bool,
    /// The compositor has fullscreened this toplevel.
    pub fullscreen: bool,
    /// The compositor is interactively resizing.
    pub resizing: bool,
    /// The toplevel is active (has focus).
    pub activated: bool,
    /// The toplevel is suspended (not visible to the user).
    pub suspended: bool,
    /// Edges adjacent to another part of the tiling grid.
    pub tiled: Edges,
    /// Edges the toplevel should not resize from.
    pub constrained: Edges,
    /// Current width, in surface-local pixels.
    pub width: i32,
    /// Current height, in surface-local pixels.
    pub height: i32,
    /// Maximum width the client asked for, `0` if unbounded.
    pub max_width: i32,
    /// Maximum height the client asked for, `0` if unbounded.
    pub max_height: i32,
    /// Minimum width the client asked for, `0` if unbounded.
    pub min_width: i32,
    /// Minimum height the client asked for, `0` if unbounded.
    pub min_height: i32,
}

impl ToplevelState {
    /// Copy wlroots' `wlr_xdg_toplevel_state` out.
    ///
    /// # Safety
    ///
    /// `state` must point at a live, initialised `wlr_xdg_toplevel_state`.
    /// Only reads.
    unsafe fn from_c(state: &sys::wlr_xdg_toplevel_state) -> ToplevelState {
        ToplevelState {
            maximized: state.maximized,
            fullscreen: state.fullscreen,
            resizing: state.resizing,
            activated: state.activated,
            suspended: state.suspended,
            tiled: Edges::from_xdg(state.tiled),
            constrained: Edges::from_xdg(state.constrained),
            width: state.width,
            height: state.height,
            max_width: state.max_width,
            max_height: state.max_height,
            min_width: state.min_width,
            min_height: state.min_height,
        }
    }
}

/// A snapshot of the state a client **requested** through
/// `xdg_toplevel.set_maximized`/`set_minimized`/`set_fullscreen`, read from
/// `wlr_xdg_toplevel.requested`.
///
/// These are requests, not applied state: a compositor is free to decline, and
/// xdg-shell still requires it answer with a configure. Copied out for the
/// same reason [`ToplevelState`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct ToplevelRequested {
    /// The client asked to maximize.
    pub maximized: bool,
    /// The client asked to minimize.
    pub minimized: bool,
    /// The client asked to fullscreen.
    pub fullscreen: bool,
}

impl ToplevelRequested {
    /// Copy the three plain booleans out of wlroots' struct.
    ///
    /// # Safety
    ///
    /// `requested` must point at a live, initialised
    /// `wlr_xdg_toplevel_requested`. Only the three booleans are read; the
    /// `fullscreen_output` pointer and the embedded listener are deliberately
    /// never touched.
    unsafe fn from_c(requested: &sys::wlr_xdg_toplevel_requested) -> ToplevelRequested {
        ToplevelRequested {
            maximized: requested.maximized,
            minimized: requested.minimized,
            fullscreen: requested.fullscreen,
        }
    }
}

/// Decode a `top`=1/`bottom`=2/`left`=4/`right`=8 edge bitmask into its four
/// booleans, in that order.
///
/// Shared by [`Edges::from_xdg`] and
/// [`crate::layer::Anchor::from_bits`](crate::layer::Anchor::from_bits):
/// xdg-shell's `xdg_toplevel_resize_edge` and wlr-layer-shell's
/// `zwlr_layer_surface_v1_anchor` happen to assign the identical four bits to
/// the identical four edges, so the decode is one function with two distinct
/// public result types built from it, rather than the same four lines
/// maintained twice. `pub(crate)` — both call sites live in this crate; there
/// is no reason for a consumer to decode this bitmask itself.
pub(crate) fn decode_edge_bits(bits: u32) -> (bool, bool, bool, bool) {
    (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0)
}

/// Copy a wlroots-owned C string field out, or `None` if it is null.
///
/// # Safety
///
/// `p` must be null or a live, NUL-terminated C string owned by wlroots.
unsafe fn cstr_field(p: *mut std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `p` is a live NUL-terminated string; this
    // copies it out and never frees it.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::{Edges, Toplevel, ToplevelId, WmCapabilities};
    use crate::sys;
    use std::alloc::{Layout, alloc_zeroed, dealloc};

    /// A zeroed, heap-allocated `wlr_xdg_toplevel` chained to a fabricated
    /// `wlr_xdg_surface` and `wlr_surface`, wired up just enough for
    /// [`current_size`](Toplevel::current_size) to read a real value through
    /// the pointer chain its own doc names (`base` -> `base->surface` ->
    /// `surface->current`).
    ///
    /// `alloc_zeroed` rather than `std::mem::zeroed`, for the same reason
    /// `output.rs`'s `ScratchOutput` uses it: these structs embed
    /// `wl_signal`/`wl_listener` machinery with bare function pointers, a bit
    /// pattern `std::mem::zeroed` refuses to produce as a materialised
    /// *value*. Allocating the bytes directly and only ever touching them
    /// through a raw pointer -- never loading a whole struct into a Rust
    /// place -- sidesteps that: nothing here reads a listener's `notify`
    /// field or any `wl_list` link, only the plain `i32`s `current_size`
    /// itself reads.
    struct ScratchToplevel {
        toplevel: *mut sys::wlr_xdg_toplevel,
        surface_role: *mut sys::wlr_xdg_surface,
        surface: *mut sys::wlr_surface,
    }

    impl ScratchToplevel {
        fn new(width: i32, height: i32) -> Self {
            fn alloc_zeroed_of<T>() -> *mut T {
                let layout = Layout::new::<T>();
                // SAFETY: every type this is called with here has a nonzero
                // size, so `alloc_zeroed` returns either null (checked
                // below) or a suitably aligned, zeroed allocation of exactly
                // that size.
                let ptr = unsafe { alloc_zeroed(layout) }.cast::<T>();
                assert!(!ptr.is_null(), "allocation failed");
                ptr
            }

            let surface = alloc_zeroed_of::<sys::wlr_surface>();
            let surface_role = alloc_zeroed_of::<sys::wlr_xdg_surface>();
            let toplevel = alloc_zeroed_of::<sys::wlr_xdg_toplevel>();

            // SAFETY: each pointer is a fresh, exclusively-owned, zeroed
            // allocation sized for its own type; these writes only touch the
            // specific fields named below, all in bounds of that allocation.
            unsafe {
                (*surface).current.width = width;
                (*surface).current.height = height;
                (*surface_role).surface = surface;
                (*toplevel).base = surface_role;
            }

            Self {
                toplevel,
                surface_role,
                surface,
            }
        }

        /// # Safety
        ///
        /// The returned handle borrows this `ScratchToplevel`'s allocations
        /// and must not outlive it. Its id is never read by `current_size`,
        /// so the fixed placeholder here is fine for this narrow use.
        unsafe fn toplevel(&self) -> Toplevel<'_> {
            // SAFETY: `self.toplevel` is a live, fully-wired allocation for
            // as long as `self` is; the caller upholds the lifetime bound.
            unsafe { Toplevel::from_raw_with_id(self.toplevel, ToplevelId(0)) }
        }
    }

    impl Drop for ScratchToplevel {
        fn drop(&mut self) {
            // SAFETY: each pointer was allocated by `alloc_zeroed` with the
            // matching `Layout::new::<T>()` in `new`, is still exclusively
            // owned by this struct, and nothing else frees or aliases it.
            unsafe {
                dealloc(self.toplevel.cast(), Layout::new::<sys::wlr_xdg_toplevel>());
                dealloc(
                    self.surface_role.cast(),
                    Layout::new::<sys::wlr_xdg_surface>(),
                );
                dealloc(self.surface.cast(), Layout::new::<sys::wlr_surface>());
            }
        }
    }

    #[test]
    fn current_size_reads_the_surfaces_current_committed_size() {
        let scratch = ScratchToplevel::new(1920, 1080);
        // SAFETY: `scratch` outlives every use of the handle below.
        let toplevel = unsafe { scratch.toplevel() };
        assert_eq!(toplevel.current_size(), (1920, 1080));
    }

    /// The documented pre-first-commit default: `wlr_surface_state` starts
    /// zeroed, and `current_size` reads it directly rather than tracking a
    /// separate "has it mapped yet" flag of its own.
    #[test]
    fn current_size_is_zero_before_any_commit() {
        let scratch = ScratchToplevel::new(0, 0);
        // SAFETY: as above.
        let toplevel = unsafe { scratch.toplevel() };
        assert_eq!(toplevel.current_size(), (0, 0));
    }

    #[test]
    fn from_xdg_decodes_every_bit_and_their_combinations() {
        assert_eq!(Edges::from_xdg(0), Edges::default());
        assert_eq!(
            Edges::from_xdg(1),
            Edges {
                top: true,
                ..Default::default()
            }
        );
        assert_eq!(
            Edges::from_xdg(2),
            Edges {
                bottom: true,
                ..Default::default()
            }
        );
        assert_eq!(
            Edges::from_xdg(4),
            Edges {
                left: true,
                ..Default::default()
            }
        );
        assert_eq!(
            Edges::from_xdg(8),
            Edges {
                right: true,
                ..Default::default()
            }
        );
        // top-left corner: bits ORed together.
        assert_eq!(
            Edges::from_xdg(1 | 4),
            Edges {
                top: true,
                left: true,
                ..Default::default()
            }
        );
    }

    /// `Edges` is this crate's spelling of `enum wlr_edges` as well as of
    /// xdg-shell's resize-edge bitmask — wlroots assigns the same four bits to
    /// the same four edges, which is what lets one type serve both. Pin that
    /// against wlroots' own constants rather than against the comment saying
    /// so: if a future wlroots renumbered `wlr_edges`, `Edges` would silently
    /// start decoding it wrong everywhere it is used.
    #[test]
    fn edge_bit_values_match_the_wlroots_header() {
        assert_eq!(sys::wlr_edges::WLR_EDGE_NONE.0, 0);
        assert_eq!(
            Edges::from_xdg(sys::wlr_edges::WLR_EDGE_NONE.0),
            Edges::default()
        );

        for (bits, expected) in [
            (sys::wlr_edges::WLR_EDGE_TOP, (true, false, false, false)),
            (sys::wlr_edges::WLR_EDGE_BOTTOM, (false, true, false, false)),
            (sys::wlr_edges::WLR_EDGE_LEFT, (false, false, true, false)),
            (sys::wlr_edges::WLR_EDGE_RIGHT, (false, false, false, true)),
        ] {
            let (top, bottom, left, right) = expected;
            assert_eq!(
                Edges::from_xdg(bits.0),
                Edges {
                    top,
                    bottom,
                    left,
                    right
                },
                "wlr_edges bit {}",
                bits.0
            );
        }

        // Every edge at once, which is also the whole of the enum's domain.
        let all = sys::wlr_edges::WLR_EDGE_TOP.0
            | sys::wlr_edges::WLR_EDGE_BOTTOM.0
            | sys::wlr_edges::WLR_EDGE_LEFT.0
            | sys::wlr_edges::WLR_EDGE_RIGHT.0;
        assert_eq!(all, 0b1111);
        assert!(!Edges::from_xdg(all).is_empty());
    }

    /// Both test constructors sit at the top of the id space, which a
    /// process-wide counter starting at 1 and only ever incrementing can
    /// never reach -- so neither can collide with a real toplevel's id.
    #[test]
    fn test_constructors_never_collide_with_a_real_id() {
        assert!(ToplevelId::dangling_for_test().0 > u32::MAX as u64);
        for n in 1..=8 {
            assert!(ToplevelId::dangling_nth_for_test(n).0 > u32::MAX as u64);
        }
    }

    /// The whole point of `dangling_nth_for_test`: distinct `n` must produce
    /// distinct ids, unlike `dangling_for_test`'s single fixed value.
    ///
    /// `n = 0` is excluded: `u64::MAX - 0` is `u64::MAX`, the same value
    /// `dangling_for_test` returns, so callers wanting an id distinct from
    /// every other test id (including `dangling_for_test`'s) must start `n`
    /// at 1 -- which is what `ToplevelKey::for_test`'s consumers do.
    #[test]
    fn nth_for_test_is_distinguishable_by_n() {
        let ids: Vec<ToplevelId> = (1..=8).map(ToplevelId::dangling_nth_for_test).collect();
        for i in 0..ids.len() {
            for j in 0..ids.len() {
                assert_eq!(i == j, ids[i] == ids[j]);
            }
        }
    }

    /// `dangling_nth_for_test` (for `n >= 1`) must not collide with
    /// `dangling_for_test` either, since a test might reasonably use both in
    /// the same suite.
    #[test]
    fn nth_for_test_does_not_collide_with_dangling_for_test() {
        let base = ToplevelId::dangling_for_test();
        for n in 1..=8 {
            assert_ne!(base, ToplevelId::dangling_nth_for_test(n));
        }
    }

    /// The ledgered fix: an `n` near `u64::MAX` must not wrap past the
    /// reserved band and alias a value a real (small, counter-issued) id
    /// could ever reach. Before the `% (1 << 32)` fold,
    /// `dangling_nth_for_test(u64::MAX)` computed `u64::MAX - u64::MAX == 0`
    /// — well inside real-id space, since ids start at 1 and count up — which
    /// made the "no live toplevel can have this id" doc guarantee false for
    /// that call.
    #[test]
    fn nth_for_test_stays_in_band_at_the_extremes() {
        // The exact band the fold promises: `u64::MAX - (n % 2^32)` can
        // never land below `u64::MAX - (2^32 - 1)`, so a tighter bound than
        // "somewhere above u32::MAX" is both possible and worth asserting —
        // it is the difference between "still large" and "provably inside
        // the documented 2^32-wide reserved band".
        let band_floor = u64::MAX - ((1u64 << 32) - 1);
        for n in [u64::MAX, u64::MAX - 5] {
            let id = ToplevelId::dangling_nth_for_test(n);
            assert!(
                id.0 >= band_floor,
                "n = {n} produced {:#x}, which is below the reserved band's floor {:#x}",
                id.0,
                band_floor
            );
        }
    }

    /// `n = 0` must still alias `dangling_for_test`'s value after the fold —
    /// the documented behaviour the fix is required to preserve exactly.
    #[test]
    fn nth_for_test_zero_still_aliases_dangling_for_test() {
        assert_eq!(
            ToplevelId::dangling_nth_for_test(0),
            ToplevelId::dangling_for_test()
        );
    }

    /// `WmCapabilities`' four bits are the ones `enum
    /// wlr_xdg_toplevel_wm_capabilities` declares. Pinning them against the
    /// generated constants is what makes the hand-rolled bitmask a checked
    /// decision: a protocol renumbering changes the constants and this test
    /// fails rather than every advertisement being wrong in silence.
    #[test]
    fn wm_capability_bits_are_the_ones_the_protocol_declares() {
        use sys::wlr_xdg_toplevel_wm_capabilities as C;
        assert_eq!(
            WmCapabilities::WINDOW_MENU.bits(),
            C::WLR_XDG_TOPLEVEL_WM_CAPABILITIES_WINDOW_MENU.0
        );
        assert_eq!(
            WmCapabilities::MAXIMIZE.bits(),
            C::WLR_XDG_TOPLEVEL_WM_CAPABILITIES_MAXIMIZE.0
        );
        assert_eq!(
            WmCapabilities::FULLSCREEN.bits(),
            C::WLR_XDG_TOPLEVEL_WM_CAPABILITIES_FULLSCREEN.0
        );
        assert_eq!(
            WmCapabilities::MINIMIZE.bits(),
            C::WLR_XDG_TOPLEVEL_WM_CAPABILITIES_MINIMIZE.0
        );
        assert_eq!(
            (WmCapabilities::MAXIMIZE | WmCapabilities::MINIMIZE).bits(),
            C::WLR_XDG_TOPLEVEL_WM_CAPABILITIES_MAXIMIZE.0
                | C::WLR_XDG_TOPLEVEL_WM_CAPABILITIES_MINIMIZE.0
        );
        let both = WmCapabilities::MAXIMIZE | WmCapabilities::MINIMIZE;
        assert!(both.contains(WmCapabilities::MAXIMIZE));
        assert!(both.contains(WmCapabilities::MINIMIZE));
        assert!(!WmCapabilities::MAXIMIZE.contains(WmCapabilities::MINIMIZE));
    }

    /// `Edges::to_xdg` is the exact inverse of `from_xdg` over the whole
    /// domain, which is what the tiled/constrained setters rely on.
    #[test]
    fn edges_round_trip_through_the_xdg_bitmask() {
        for bits in [0u32, 1, 2, 4, 8, 0b1111, 0b0101] {
            assert_eq!(Edges::from_xdg(bits).to_xdg(), bits, "bits {bits:#b}");
        }
    }
}
