//! Borrow-scoped xdg-popup handles, their stable ids, and the client's
//! positioner.
//!
//! Same shape as [`Toplevel`](crate::Toplevel), for the same reason: a
//! `wlr_xdg_popup` is freed whenever its client says so, so a handle that
//! escapes the handler it was passed to is a use-after-free. The lifetime and
//! the private constructor make that a compile error.
//!
//! The id is attached with `wlr_addon` to the popup's **`wlr_surface`**
//! (`popup->base->surface`), not to the popup itself: `wlr_xdg_popup` has no
//! addon set, `wlr_surface` does, and the two die together — the identical
//! argument [`crate::ToplevelId`]'s module doc makes.
//!
//! # What this module deliberately does not do
//!
//! It does not create, inspect, end or imitate a `wlr_xdg_popup_grab`. wlroots
//! builds one itself when a client sends `xdg_popup.grab`, installs three seat
//! grabs (pointer, keyboard, touch), routes delivery to the popup chain,
//! dismisses the chain on a press outside it, and restores the pre-grab
//! keyboard focus when the grab ends. There is no C API to touch any of that —
//! the export table has no `wlr_xdg_popup_grab_*` symbol at all — and a second
//! `wlr_seat_pointer_start_grab` would displace wlroots' own and break chain
//! dismissal. What this crate offers instead is observation:
//! [`Popup::grab_requested`] (`popup->seat != NULL`) and
//! [`Runtime::seat_has_explicit_grab`](crate::Runtime::seat_has_explicit_grab).

// This task lands the ids, the parent enum and the positioner enums only;
// nothing in the rest of the crate references them yet (`lib.rs` gains its
// `pub use popup::{…}` re-export, and `runtime.rs`/`backend.rs` their callers,
// in later tasks of this same part). Outside `#[cfg(test)]` — which is the
// only consumer so far — every item below is therefore unreachable, exactly
// the situation `Output::from_raw` documents its own
// `#[cfg_attr(not(test), allow(dead_code))]` for. Remove this once a
// non-test caller exists.
#![cfg_attr(not(test), allow(dead_code, unused_imports))]

use crate::id::next_id;
use crate::{LayerSurfaceId, ToplevelId, sys};

/// Identifies one live `wlr_xdg_popup` for as long as the consumer chooses to
/// remember it.
///
/// Storable, comparable and hashable — unlike a handle. Minted on the popup's
/// `wlr_surface` addon set from the same process-wide counter as
/// [`ToplevelId`], [`LayerSurfaceId`] and [`NodeId`](crate::NodeId), so ids
/// never collide across kinds.
///
/// Deliberately no `PartialOrd`/`Ord`: an opaque id's ordering would promise
/// creation-order semantics nobody asked for, and this API is frozen within the
/// wlroots minor, so a derive added here could not be withdrawn. See
/// [`ToplevelId`], whose doc this mirrors, for the full argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PopupId(pub(crate) u64);

impl PopupId {
    /// An id no live popup can have, for testing the "unknown id" path.
    ///
    /// Public for the reason [`ToplevelId::dangling_for_test`] is: "every
    /// by-id operation reports a miss rather than dereferencing" is a promise
    /// to consumers, and a promise nobody can write a test for is not one.
    /// Ids come from a counter that starts at 1 and only increments, so
    /// `u64::MAX` cannot be handed to a real popup.
    pub fn dangling_for_test() -> PopupId {
        PopupId(u64::MAX)
    }

    /// A distinct id no live popup can have, for testing.
    ///
    /// `n` is folded into a fixed 2^32-wide band immediately below `u64::MAX`
    /// rather than subtracted unclamped, so even `n = u64::MAX` lands on a
    /// value the shared counter cannot reach — the identical argument
    /// [`ToplevelId::dangling_nth_for_test`] makes. `n = 0` aliases
    /// [`dangling_for_test`](Self::dangling_for_test).
    pub fn dangling_nth_for_test(n: u64) -> PopupId {
        PopupId(u64::MAX - (n % (1u64 << 32)))
    }

    /// Mint a fresh id from the crate-wide counter, for the tables' own tests.
    ///
    /// Not public: a real popup's id comes from its surface's addon set, which
    /// is what makes wlroots release it at exactly the right moment.
    #[cfg(test)]
    #[allow(dead_code)] // wired up by a later task in this part
    pub(crate) fn next_for_test() -> PopupId {
        PopupId(next_id())
    }
}

/// What a popup hangs off.
///
/// A layer-shell popup is created with a **NULL** xdg parent
/// (`xdg_surface.get_popup` with `parent = null`) and only then reparented by
/// `zwlr_layer_surface_v1.get_popup`, so its parent is knowable *only* from the
/// parent-scoped `new_popup` signal — never from `popup->parent`, which is null
/// at creation. This crate therefore records the parent once, at announcement
/// time, and [`Popup::parent`] returns that recording rather than re-deriving
/// anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PopupParent {
    /// An `xdg_toplevel`.
    Toplevel(ToplevelId),
    /// A `zwlr_layer_surface_v1`.
    Layer(LayerSurfaceId),
    /// Another popup — a nested chain (a submenu off a menu).
    Popup(PopupId),
}

impl PopupParent {
    /// Whether this parent is itself a popup, i.e. whether the popup naming it
    /// is part of a nested chain rather than hanging directly off a window.
    #[must_use]
    pub fn is_popup(self) -> bool {
        matches!(self, PopupParent::Popup(_))
    }
}

/// `xdg_positioner.set_anchor` — which point of the anchor rectangle the popup
/// is positioned against.
///
/// `#[non_exhaustive]`: the protocol may add anchors, and a value this crate
/// does not know maps to [`PositionerAnchor::None`] rather than panicking. The
/// value comes from an untrusted client, so that is a hard rule, not a
/// courtesy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PositionerAnchor {
    /// The centre of the anchor rectangle.
    None,
    /// The middle of its top edge.
    Top,
    /// The middle of its bottom edge.
    Bottom,
    /// The middle of its left edge.
    Left,
    /// The middle of its right edge.
    Right,
    /// Its top-left corner.
    TopLeft,
    /// Its bottom-left corner.
    BottomLeft,
    /// Its top-right corner.
    TopRight,
    /// Its bottom-right corner.
    BottomRight,
}

impl PositionerAnchor {
    /// Convert from `enum xdg_positioner_anchor`.
    ///
    /// An unrecognised value yields [`PositionerAnchor::None`], which is the
    /// protocol's own initial value, and never panics — the value crosses the
    /// wire from a client this compositor does not control. Nothing is logged:
    /// this crate binds no Rust-side logging symbol (wlroots' `wlr_log` is a
    /// `static inline` macro over an unbound `_wlr_log`, and there is
    /// deliberately no `log`/`tracing` dependency), the same reason
    /// `Runtime::apply_cursor` gives for its own silent fallback.
    ///
    /// Matched on the raw `u32` rather than on the newtype's associated
    /// constants, and the mapping is pinned by
    /// `the_anchor_values_are_the_ones_the_protocol_declares` — the same
    /// discipline `DataPtrAccess`'s own test applies.
    #[allow(dead_code)] // wired up by a later task in this part
    pub(crate) fn from_raw(raw: u32) -> PositionerAnchor {
        match raw {
            1 => PositionerAnchor::Top,
            2 => PositionerAnchor::Bottom,
            3 => PositionerAnchor::Left,
            4 => PositionerAnchor::Right,
            5 => PositionerAnchor::TopLeft,
            6 => PositionerAnchor::BottomLeft,
            7 => PositionerAnchor::TopRight,
            8 => PositionerAnchor::BottomRight,
            // 0 is `NONE`; anything else is a client sending a value this
            // protocol version does not define.
            _ => PositionerAnchor::None,
        }
    }
}

/// `xdg_positioner.set_gravity` — which direction the popup extends from its
/// anchor point.
///
/// `#[non_exhaustive]` for the reason [`PositionerAnchor`] is, and with the same
/// unknown-value rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PositionerGravity {
    /// Centred on the anchor point.
    None,
    /// Extends upward.
    Top,
    /// Extends downward.
    Bottom,
    /// Extends leftward.
    Left,
    /// Extends rightward.
    Right,
    /// Extends up and left.
    TopLeft,
    /// Extends down and left.
    BottomLeft,
    /// Extends up and right.
    TopRight,
    /// Extends down and right.
    BottomRight,
}

impl PositionerGravity {
    /// Convert from `enum xdg_positioner_gravity`; unknown values yield
    /// [`PositionerGravity::None`]. See [`PositionerAnchor::from_raw`] for the
    /// full argument, which applies here verbatim.
    #[allow(dead_code)] // wired up by a later task in this part
    pub(crate) fn from_raw(raw: u32) -> PositionerGravity {
        match raw {
            1 => PositionerGravity::Top,
            2 => PositionerGravity::Bottom,
            3 => PositionerGravity::Left,
            4 => PositionerGravity::Right,
            5 => PositionerGravity::TopLeft,
            6 => PositionerGravity::BottomLeft,
            7 => PositionerGravity::TopRight,
            8 => PositionerGravity::BottomRight,
            _ => PositionerGravity::None,
        }
    }
}

/// `xdg_positioner.set_constraint_adjustment` — which rearrangements the client
/// permits when the popup would fall outside the constraint box.
///
/// A bitmask of `enum xdg_positioner_constraint_adjustment`. Hand-rolled rather
/// than a `bitflags` dependency, following
/// [`DataPtrAccess`](crate::DataPtrAccess) and `BufferCaps` before it: the six
/// bits are the whole domain and their values are pinned by this module's own
/// tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ConstraintAdjustment(u32);

impl ConstraintAdjustment {
    /// No rearrangement is permitted; the popup is placed as asked even if it
    /// falls outside. The protocol's initial value, and [`Default`].
    pub const NONE: ConstraintAdjustment = ConstraintAdjustment(0);
    /// The popup may be slid along the x axis to fit.
    pub const SLIDE_X: ConstraintAdjustment = ConstraintAdjustment(1);
    /// The popup may be slid along the y axis to fit.
    pub const SLIDE_Y: ConstraintAdjustment = ConstraintAdjustment(2);
    /// The popup may be flipped to the opposite side of its anchor on x.
    pub const FLIP_X: ConstraintAdjustment = ConstraintAdjustment(4);
    /// The popup may be flipped to the opposite side of its anchor on y.
    pub const FLIP_Y: ConstraintAdjustment = ConstraintAdjustment(8);
    /// The popup may be narrowed to fit.
    pub const RESIZE_X: ConstraintAdjustment = ConstraintAdjustment(16);
    /// The popup may be shortened to fit.
    pub const RESIZE_Y: ConstraintAdjustment = ConstraintAdjustment(32);

    /// Whether **every** bit of `other` is set here — not "any", which is the
    /// distinction that decides whether a compositor may flip a popup the
    /// client only permitted it to slide.
    #[must_use]
    pub fn contains(self, other: ConstraintAdjustment) -> bool {
        self.0 & other.0 == other.0
    }

    /// The raw mask, as the protocol numbers it.
    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Build from a raw `enum xdg_positioner_constraint_adjustment` value.
    ///
    /// Bits this crate does not know are **kept**, not dropped: the mask is
    /// handed straight back to wlroots by
    /// [`PositionerRules::unconstrain_box`], which is the code that interprets
    /// it, and silently clearing a bit here would change the client's request
    /// rather than merely failing to describe it. Nothing can panic — it is one
    /// integer.
    #[allow(dead_code)] // wired up by a later task in this part
    pub(crate) fn from_raw(raw: u32) -> ConstraintAdjustment {
        ConstraintAdjustment(raw)
    }
}

impl std::ops::BitOr for ConstraintAdjustment {
    type Output = ConstraintAdjustment;

    fn bitor(self, rhs: ConstraintAdjustment) -> ConstraintAdjustment {
        ConstraintAdjustment(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for ConstraintAdjustment {
    fn bitor_assign(&mut self, rhs: ConstraintAdjustment) {
        self.0 |= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ids come from the same process-wide counter every other id type in
    /// this crate uses, so a popup id can never collide with a toplevel or
    /// layer-surface id — but it *can* be mislabelled, which is what
    /// `toplevel_id_of_surface`'s role check exists to stop. This pins the
    /// non-collision half.
    #[test]
    fn the_dangling_ids_are_distinct_and_out_of_the_issued_range() {
        assert_eq!(
            PopupId::dangling_nth_for_test(0),
            PopupId::dangling_for_test()
        );
        assert_ne!(
            PopupId::dangling_nth_for_test(1),
            PopupId::dangling_for_test()
        );
        assert_ne!(
            PopupId::dangling_nth_for_test(1),
            PopupId::dangling_nth_for_test(2)
        );
        // The band is 2^32 wide immediately below u64::MAX, so even an absurd
        // `n` stays in the reserved range rather than wrapping into the
        // counter's own.
        assert!(PopupId::dangling_nth_for_test(u64::MAX).0 > u64::MAX - (1u64 << 32) - 1);
    }

    #[test]
    fn a_popup_parent_knows_whether_it_is_itself_a_popup() {
        assert!(PopupParent::Popup(PopupId::dangling_for_test()).is_popup());
        assert!(!PopupParent::Toplevel(ToplevelId::dangling_for_test()).is_popup());
        assert!(!PopupParent::Layer(LayerSurfaceId::dangling_for_test()).is_popup());
    }

    /// The six bits are `1, 2, 4, 8, 16, 32`, exactly as
    /// `enum xdg_positioner_constraint_adjustment` declares them. This is what
    /// makes hand-rolling the bitmask (rather than taking a `bitflags`
    /// dependency — see this crate's `DataPtrAccess` for the standing decision)
    /// a *checked* decision: the constants are re-exported from `wlr-sys`, so a
    /// protocol renumbering would change them silently and only this test would
    /// notice.
    #[test]
    fn the_constraint_bits_are_the_ones_the_protocol_declares() {
        assert_eq!(ConstraintAdjustment::NONE.bits(), 0);
        assert_eq!(ConstraintAdjustment::SLIDE_X.bits(), 1);
        assert_eq!(ConstraintAdjustment::SLIDE_Y.bits(), 2);
        assert_eq!(ConstraintAdjustment::FLIP_X.bits(), 4);
        assert_eq!(ConstraintAdjustment::FLIP_Y.bits(), 8);
        assert_eq!(ConstraintAdjustment::RESIZE_X.bits(), 16);
        assert_eq!(ConstraintAdjustment::RESIZE_Y.bits(), 32);

        assert_eq!(
            ConstraintAdjustment::SLIDE_X.bits(),
            sys::xdg_positioner_constraint_adjustment::XDG_POSITIONER_CONSTRAINT_ADJUSTMENT_SLIDE_X
                .0
        );
        assert_eq!(
            ConstraintAdjustment::FLIP_Y.bits(),
            sys::xdg_positioner_constraint_adjustment::XDG_POSITIONER_CONSTRAINT_ADJUSTMENT_FLIP_Y
                .0
        );
        assert_eq!(
            ConstraintAdjustment::RESIZE_Y.bits(),
            sys::xdg_positioner_constraint_adjustment::XDG_POSITIONER_CONSTRAINT_ADJUSTMENT_RESIZE_Y
                .0
        );
    }

    /// `contains` is "every bit of the argument is set here", not "any" — the
    /// distinction that decides whether a compositor may flip a popup it may
    /// only slide. Same semantics `DataPtrAccess::contains` pins.
    #[test]
    fn contains_is_every_bit_not_any_bit() {
        let both = ConstraintAdjustment::FLIP_X | ConstraintAdjustment::FLIP_Y;
        assert_eq!(both.bits(), 0b1100);
        assert!(both.contains(ConstraintAdjustment::FLIP_X));
        assert!(both.contains(ConstraintAdjustment::FLIP_Y));
        assert!(both.contains(both));
        assert!(!both.contains(ConstraintAdjustment::SLIDE_X));
        assert!(!both.contains(ConstraintAdjustment::FLIP_X | ConstraintAdjustment::SLIDE_X));
        // The empty set is contained in everything, including itself.
        assert!(both.contains(ConstraintAdjustment::NONE));
        assert!(ConstraintAdjustment::NONE.contains(ConstraintAdjustment::NONE));
        assert!(!ConstraintAdjustment::NONE.contains(ConstraintAdjustment::FLIP_X));
    }

    #[test]
    fn bit_or_assign_accumulates() {
        let mut c = ConstraintAdjustment::NONE;
        c |= ConstraintAdjustment::SLIDE_X;
        c |= ConstraintAdjustment::SLIDE_Y;
        assert_eq!(c.bits(), 0b11);
    }

    /// The nine anchors and nine gravities are numbered `0..=8` in
    /// `xdg-shell.xml`, and `from_raw`'s match arms hard-code those numbers.
    /// Pinning them against the generated constants is what makes hard-coding
    /// them safe: a protocol renumbering changes the constants and this test
    /// fails, rather than every popup in the session being anchored wrongly.
    #[test]
    fn the_anchor_values_are_the_ones_the_protocol_declares() {
        use sys::xdg_positioner_anchor as A;
        use sys::xdg_positioner_gravity as G;

        assert_eq!(
            PositionerAnchor::from_raw(A::XDG_POSITIONER_ANCHOR_NONE.0),
            PositionerAnchor::None
        );
        assert_eq!(
            PositionerAnchor::from_raw(A::XDG_POSITIONER_ANCHOR_TOP.0),
            PositionerAnchor::Top
        );
        assert_eq!(
            PositionerAnchor::from_raw(A::XDG_POSITIONER_ANCHOR_BOTTOM_RIGHT.0),
            PositionerAnchor::BottomRight
        );
        assert_eq!(
            PositionerGravity::from_raw(G::XDG_POSITIONER_GRAVITY_NONE.0),
            PositionerGravity::None
        );
        assert_eq!(
            PositionerGravity::from_raw(G::XDG_POSITIONER_GRAVITY_BOTTOM_LEFT.0),
            PositionerGravity::BottomLeft
        );
        assert_eq!(
            PositionerGravity::from_raw(G::XDG_POSITIONER_GRAVITY_TOP_RIGHT.0),
            PositionerGravity::TopRight
        );
    }
}
