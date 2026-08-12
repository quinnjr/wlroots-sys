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

use crate::sys;

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
            let mut pid: sys::pid_t = 0;
            let mut uid: u32 = 0;
            let mut gid: u32 = 0;
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
}

/// Which edge(s) of a toplevel an interactive resize is dragging.
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
        Edges {
            top: bits & 1 != 0,
            bottom: bits & 2 != 0,
            left: bits & 4 != 0,
            right: bits & 8 != 0,
        }
    }
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
    use super::{Edges, ToplevelId};

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
}
