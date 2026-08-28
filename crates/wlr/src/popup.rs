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

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::{Box2D, LayerSurfaceId, ToplevelId, sys};

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

    /// The chain root: walks `Popup(_)` links down to the [`Toplevel`] or
    /// [`Layer`] at the bottom.
    ///
    /// `Some(self)` when `self` is already a root. `None` when a link is
    /// already dead — the by-id miss this crate promises everywhere — and also
    /// `None` if the walk exceeds
    /// [`Runtime::MAX_POPUP_DEPTH`](crate::Runtime), which cannot happen with a
    /// table wlroots produced but is what keeps a corrupted one from hanging
    /// the compositor's input path.
    ///
    /// [`Toplevel`]: PopupParent::Toplevel
    /// [`Layer`]: PopupParent::Layer
    #[must_use]
    pub fn root(self, rt: &crate::Runtime) -> Option<PopupParent> {
        let mut cursor = self;
        for _ in 0..crate::Runtime::MAX_POPUP_DEPTH {
            match cursor {
                PopupParent::Popup(id) => cursor = rt.popup_parent(id)?,
                root => return Some(root),
            }
        }
        None
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

/// A snapshot of the client's `xdg_positioner`, copied out of
/// `wlr_xdg_positioner_rules`.
///
/// **Copied, never borrowed.** Every accessor that hands one of these back
/// releases wlroots' memory before returning, because the caller will re-enter
/// wlroots — configuring, dismissing, positioning — which can emit a signal,
/// which can destroy the very popup the rules were read from. A borrowed view
/// would be a use-after-free the borrow checker could not see, since the
/// lifetime would be tied to the handle rather than to wlroots' own decisions.
///
/// The two methods are FFI calls into wlroots rather than the placement algebra
/// they look like, for the reason [`geom`](crate::Box2D)'s module doc gives
/// about `wlr_box` predicates: xdg-shell's anchor/gravity/adjustment rules have
/// edge cases whose answers are not the obvious ones, and a reimplementation is
/// free to drift from wlroots' answer, silently, in a patch release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionerRules {
    /// `xdg_positioner.set_anchor_rect`, in the parent's window-geometry space.
    pub anchor_rect: Box2D,
    /// `xdg_positioner.set_anchor`.
    pub anchor: PositionerAnchor,
    /// `xdg_positioner.set_gravity`.
    pub gravity: PositionerGravity,
    /// `xdg_positioner.set_constraint_adjustment`.
    pub constraint_adjustment: ConstraintAdjustment,
    /// `xdg_positioner.set_size`, as `(width, height)`.
    pub size: (i32, i32),
    /// `xdg_positioner.set_parent_size` (protocol v3+). `Some` only when the
    /// client actually sent one — see this type's own round-trip test for why
    /// a zero cannot be reported as `Some((0, 0))`.
    pub parent_size: Option<(i32, i32)>,
    /// `xdg_positioner.set_offset`.
    pub offset: (i32, i32),
    /// `xdg_positioner.set_reactive`: the client wants the popup
    /// re-unconstrained whenever its parent moves.
    pub reactive: bool,
    /// `xdg_positioner.set_parent_configure`. `Some` only when the client sent
    /// one, which wlroots flags with `has_parent_configure_serial`.
    pub parent_configure_serial: Option<u32>,
}

impl PositionerRules {
    /// `wlr_xdg_positioner_rules_get_geometry` — the **unconstrained** geometry
    /// these rules describe, in the parent surface's coordinate system.
    #[must_use]
    pub fn geometry(&self) -> Box2D {
        let rules = self.to_c();
        let mut out = Box2D::default();
        // SAFETY: `rules` is a live, exclusively-owned local of exactly the C
        // type; `out` is a live local whose layout is pinned to `wlr_box` by
        // `geom.rs`'s compile-time asserts. wlroots only reads the first and
        // only writes the second.
        unsafe {
            sys::wlr_xdg_positioner_rules_get_geometry(
                &raw const rules,
                (&raw mut out).cast::<sys::wlr_box>(),
            );
        }
        out
    }

    /// `wlr_xdg_positioner_rules_unconstrain_box` — these rules applied against
    /// a constraint box, **without touching any live popup**.
    ///
    /// The answer is whatever wlroots' own algorithm produces given the
    /// adjustment bits the client permitted: with none permitted the result is
    /// [`geometry`](Self::geometry) unchanged, however far outside the
    /// constraint that falls. This crate invents no clamp of its own.
    #[must_use]
    pub fn unconstrain_box(&self, constraint: &Box2D) -> Box2D {
        let rules = self.to_c();
        let mut out = self.geometry();
        // SAFETY: as for `geometry`, plus `constraint.as_c()` which points at
        // the caller's live `Box2D` and is only read.
        unsafe {
            sys::wlr_xdg_positioner_rules_unconstrain_box(
                &raw const rules,
                constraint.as_c(),
                (&raw mut out).cast::<sys::wlr_box>(),
            );
        }
        out
    }

    /// Copy the rules out of wlroots' own struct.
    ///
    /// # Safety
    ///
    /// `rules` must point at a live, initialised `wlr_xdg_positioner_rules`.
    /// Only reads; nothing is retained.
    pub(crate) unsafe fn from_c(rules: &sys::wlr_xdg_positioner_rules) -> PositionerRules {
        PositionerRules {
            anchor_rect: Box2D::new(
                rules.anchor_rect.x,
                rules.anchor_rect.y,
                rules.anchor_rect.width,
                rules.anchor_rect.height,
            ),
            anchor: PositionerAnchor::from_raw(rules.anchor.0),
            gravity: PositionerGravity::from_raw(rules.gravity.0),
            constraint_adjustment: ConstraintAdjustment::from_raw(rules.constraint_adjustment.0),
            size: (rules.size.width, rules.size.height),
            // A parent size the client never sent is left zeroed by wlroots,
            // and "the client asked for 0x0" is not a thing xdg-shell permits,
            // so the zero is a usable sentinel. The serial has a real flag and
            // uses it.
            parent_size: if rules.parent_size.width == 0 && rules.parent_size.height == 0 {
                None
            } else {
                Some((rules.parent_size.width, rules.parent_size.height))
            },
            offset: (rules.offset.x, rules.offset.y),
            reactive: rules.reactive,
            parent_configure_serial: if rules.has_parent_configure_serial {
                Some(rules.parent_configure_serial)
            } else {
                None
            },
        }
    }

    /// Rebuild wlroots' own struct from this copy, so the two `rules_*`
    /// functions have something to point at.
    ///
    /// Zeroed first rather than field-by-field constructed: the struct is plain
    /// data (boxes, sizes, offsets, three enums and two bools — no pointers, no
    /// `wl_listener`), so materialising a zero value is sound, unlike the
    /// role-object structs `runtime.rs`'s tests must only ever touch through a
    /// raw pointer.
    ///
    /// Takes `&self` rather than `self` despite `PositionerRules: Copy` (which
    /// is what clippy's `wrong_self_convention` wants for a `to_*` name):
    /// `geometry`/`unconstrain_box` both need to call it without giving up
    /// their own `&self`, and every call site here already has a reference in
    /// hand, so a by-value signature would only add copies at the caller.
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn to_c(&self) -> sys::wlr_xdg_positioner_rules {
        // SAFETY: every field of `wlr_xdg_positioner_rules` is an integer, a
        // bool, or a `#[repr(C)]` struct of those; none has a validity
        // requirement an all-zero pattern violates, and every field is
        // overwritten below except the padding.
        let mut rules: sys::wlr_xdg_positioner_rules = unsafe { std::mem::zeroed() };
        rules.anchor_rect = sys::wlr_box {
            x: self.anchor_rect.x,
            y: self.anchor_rect.y,
            width: self.anchor_rect.width,
            height: self.anchor_rect.height,
        };
        rules.anchor = sys::xdg_positioner_anchor(anchor_to_raw(self.anchor));
        rules.gravity = sys::xdg_positioner_gravity(gravity_to_raw(self.gravity));
        rules.constraint_adjustment =
            sys::xdg_positioner_constraint_adjustment(self.constraint_adjustment.bits());
        rules.size.width = self.size.0;
        rules.size.height = self.size.1;
        let (pw, ph) = self.parent_size.unwrap_or((0, 0));
        rules.parent_size.width = pw;
        rules.parent_size.height = ph;
        rules.offset.x = self.offset.0;
        rules.offset.y = self.offset.1;
        rules.reactive = self.reactive;
        rules.has_parent_configure_serial = self.parent_configure_serial.is_some();
        rules.parent_configure_serial = self.parent_configure_serial.unwrap_or(0);
        rules
    }
}

/// The inverse of [`PositionerAnchor::from_raw`]. Total, and pinned by the same
/// test: a variant added to the enum without a number here would not compile.
fn anchor_to_raw(anchor: PositionerAnchor) -> u32 {
    match anchor {
        PositionerAnchor::None => 0,
        PositionerAnchor::Top => 1,
        PositionerAnchor::Bottom => 2,
        PositionerAnchor::Left => 3,
        PositionerAnchor::Right => 4,
        PositionerAnchor::TopLeft => 5,
        PositionerAnchor::BottomLeft => 6,
        PositionerAnchor::TopRight => 7,
        PositionerAnchor::BottomRight => 8,
    }
}

/// The inverse of [`PositionerGravity::from_raw`]; see [`anchor_to_raw`].
fn gravity_to_raw(gravity: PositionerGravity) -> u32 {
    match gravity {
        PositionerGravity::None => 0,
        PositionerGravity::Top => 1,
        PositionerGravity::Bottom => 2,
        PositionerGravity::Left => 3,
        PositionerGravity::Right => 4,
        PositionerGravity::TopLeft => 5,
        PositionerGravity::BottomLeft => 6,
        PositionerGravity::TopRight => 7,
        PositionerGravity::BottomRight => 8,
    }
}

/// An xdg popup, borrowed for the duration of a handler call.
///
/// Same shape as [`Toplevel`](crate::Toplevel) and for the same reason: a
/// `wlr_xdg_popup` is freed whenever its client says so, so a handle that
/// escapes the handler it was passed to is a use-after-free. The lifetime and
/// the private constructor make that a compile error rather than a documented
/// rule. What you store instead is [`PopupId`].
pub struct Popup<'h> {
    raw: NonNull<sys::wlr_xdg_popup>,
    id: PopupId,
    parent: PopupParent,
    _scope: PhantomData<&'h ()>,
}

/// Hand-written rather than derived, for the same reason [`Toplevel`](crate::Toplevel)'s is: the
/// `PhantomData` scope marker has no value to print, and a raw pointer printed
/// by a derive is neither useful nor stable across runs. Named fields only:
/// `id`, `parent`, `geometry`.
impl std::fmt::Debug for Popup<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Popup")
            .field("id", &self.id)
            .field("parent", &self.parent)
            .field("geometry", &self.geometry())
            .finish()
    }
}

impl<'h> Popup<'h> {
    /// # Safety
    ///
    /// `raw` must be a live `wlr_xdg_popup` whose `base->surface` carries the id
    /// addon that produced `id`, `parent` must be the parent recorded when the
    /// popup was announced, and the returned handle must not outlive the
    /// callback it was created for.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_xdg_popup,
        id: PopupId,
        parent: PopupParent,
    ) -> Popup<'h> {
        Popup {
            raw: NonNull::new(raw).expect("wlroots handed us a null popup"),
            id,
            parent,
            _scope: PhantomData,
        }
    }
}

impl Popup<'_> {
    /// This popup's stable identity, safe to store beyond the handler.
    #[must_use]
    pub fn id(&self) -> PopupId {
        self.id
    }

    /// The parent recorded when this popup was created.
    ///
    /// **Never re-derived from `(*popup).parent` at call time.** A layer-shell
    /// popup's xdg parent is NULL at creation — the client sets it with
    /// `zwlr_layer_surface_v1.get_popup` afterwards — so reading the struct
    /// would report "no parent" for exactly the case this enum exists to
    /// distinguish.
    #[must_use]
    pub fn parent(&self) -> PopupParent {
        self.parent
    }

    /// `scheduled.rules.anchor_rect` — the rectangle the client anchored
    /// against, in the parent's window-geometry space.
    #[must_use]
    pub fn anchor_rect(&self) -> Box2D {
        self.positioner_rules().anchor_rect
    }

    /// The client's full positioner, copied out.
    ///
    /// Reads `scheduled.rules` — what the client last asked for — not `current`.
    /// See [`PositionerRules`] for why the copy is not negotiable.
    #[must_use]
    pub fn positioner_rules(&self) -> PositionerRules {
        // SAFETY: the handle's lifetime guarantees the popup is live;
        // `scheduled` is a plain embedded struct, not a pointer.
        unsafe { PositionerRules::from_c(&(*self.raw.as_ptr()).scheduled.rules) }
    }

    /// `current.geometry` — where the popup actually is, once configured, in the
    /// parent's window-geometry coordinates.
    ///
    /// Zero-sized before the first configure has been acked, which is what
    /// wlroots' own zeroed `wlr_xdg_popup_state` starts at.
    #[must_use]
    pub fn geometry(&self) -> Box2D {
        // SAFETY: the handle's lifetime guarantees the popup is live; `current`
        // is a plain embedded `wlr_xdg_popup_state`.
        let state: &sys::wlr_xdg_popup_state = unsafe { &(*self.raw.as_ptr()).current };
        Box2D::new(
            state.geometry.x,
            state.geometry.y,
            state.geometry.width,
            state.geometry.height,
        )
    }

    /// `current.reactive`: the client wants this popup re-unconstrained
    /// whenever its parent moves.
    ///
    /// A compositor honouring this re-runs
    /// [`Runtime::configure_popup`](crate::Runtime::configure_popup) from
    /// wherever it moves a window or re-arranges its layers.
    #[must_use]
    pub fn is_reactive(&self) -> bool {
        // SAFETY: as for `geometry`.
        let state: &sys::wlr_xdg_popup_state = unsafe { &(*self.raw.as_ptr()).current };
        state.reactive
    }

    /// `(*popup).seat != NULL` — the client sent `xdg_popup.grab`.
    ///
    /// **wlroots owns the grab's lifetime**; this crate only observes it. See
    /// this module's own doc for what that rules out. A `true` here means
    /// wlroots has installed pointer, keyboard and touch grabs for this popup's
    /// chain and will dismiss the chain itself on a press outside it.
    #[must_use]
    pub fn grab_requested(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the popup is live; the field
        // is only compared against null, never dereferenced.
        unsafe { !(*self.raw.as_ptr()).seat.is_null() }
    }

    /// `wlr_xdg_popup_unconstrain_from_box`: rewrite `scheduled.geometry` from
    /// the rules, fitted into `constraint`.
    ///
    /// **`constraint` is in the ROOT TOPLEVEL PARENT surface's coordinate
    /// system**, not layout or output space — wlroots' own header says so, and
    /// the compositor is what translates.
    ///
    /// **This does not merely reposition — it sends.** Contrary to what an
    /// earlier draft of this doc claimed, `wlr_xdg_popup_unconstrain_from_box`
    /// in this distribution's wlroots calls
    /// `wlr_xdg_surface_schedule_configure` itself, unconditionally, after
    /// applying the rules. That function asserts `surface->initialized`, and
    /// this distribution ships wlroots **without `NDEBUG`**, so reaching it
    /// before the popup's first commit is a hard `abort()` of the whole
    /// compositor process — exactly the hazard
    /// [`send_configure`](Self::send_configure) guards, and exactly the window a
    /// consumer sits in inside `ToplevelHandler::new_popup`. So this method
    /// carries the identical `initialized` guard and **does nothing at all**
    /// on a not-yet-initialized popup, rather than aborting.
    ///
    /// Pair it with [`send_configure`](Self::send_configure) anyway — the
    /// configure wlroots schedules from inside is the one a compositor wants,
    /// and the second call is a cheap no-op when nothing further changed. That
    /// pairing is what [`Runtime::configure_popup`](crate::Runtime::configure_popup)
    /// does.
    pub fn unconstrain(&self, constraint: &Box2D) {
        // SAFETY: the handle's lifetime guarantees the popup is live, so `base`
        // is a live `wlr_xdg_surface`; `initialized` is a plain bool.
        // `constraint.as_c()` points at the caller's live `Box2D`, whose layout
        // is pinned to `wlr_box`, and wlroots only reads it.
        unsafe {
            let base = (*self.raw.as_ptr()).base;
            if base.is_null() || !(*base).initialized {
                return;
            }
            sys::wlr_xdg_popup_unconstrain_from_box(self.raw.as_ptr(), constraint.as_c());
        }
    }

    /// `wlr_xdg_popup_get_position` — this popup's position in the **parent
    /// surface's** coordinates.
    #[must_use]
    pub fn position(&self) -> (f64, f64) {
        let mut sx = 0.0;
        let mut sy = 0.0;
        // SAFETY: the handle's lifetime guarantees the popup is live; both
        // out-parameters point at live locals that outlive the call.
        unsafe { sys::wlr_xdg_popup_get_position(self.raw.as_ptr(), &raw mut sx, &raw mut sy) };
        (sx, sy)
    }

    /// `wlr_xdg_popup_get_toplevel_coords` — a surface-local point mapped into
    /// the **root toplevel's** surface coordinates, walking the whole popup
    /// chain.
    ///
    /// This is what turns a hit inside a nested submenu into the coordinate
    /// space [`unconstrain`](Self::unconstrain)'s constraint box is expressed
    /// in.
    #[must_use]
    pub fn toplevel_coords(&self, popup_sx: i32, popup_sy: i32) -> (i32, i32) {
        let mut tx = 0;
        let mut ty = 0;
        // SAFETY: the handle's lifetime guarantees the popup is live; both
        // out-parameters point at live `c_int` locals. The C signature takes
        // `int`, not `int32_t`, which is the same type on every target this
        // crate builds for.
        unsafe {
            sys::wlr_xdg_popup_get_toplevel_coords(
                self.raw.as_ptr(),
                popup_sx,
                popup_sy,
                &raw mut tx,
                &raw mut ty,
            );
        }
        (tx, ty)
    }

    /// `wlr_xdg_surface_schedule_configure(base)`. Returns the configure serial,
    /// or `0` when the surface is not `initialized` yet — **in which case the
    /// call is skipped entirely**.
    ///
    /// That skip is not a convenience. `wlr_xdg_surface_schedule_configure`
    /// asserts `surface->initialized`, and this distribution ships wlroots
    /// **without `NDEBUG`**, so calling it before the popup's first commit is a
    /// hard `abort()` of the whole compositor process — the identical,
    /// confirmed hazard `layer.rs`'s module doc documents at length for
    /// `wlr_layer_surface_v1_configure`. The guard must never be simplified
    /// away to an unconditional send.
    pub fn send_configure(&self) -> u32 {
        // SAFETY: the handle's lifetime guarantees the popup is live, so `base`
        // is a live `wlr_xdg_surface`; `initialized` is a plain bool.
        unsafe {
            let base = (*self.raw.as_ptr()).base;
            if base.is_null() || !(*base).initialized {
                return 0;
            }
            sys::wlr_xdg_surface_schedule_configure(base)
        }
    }

    /// `scheduled.reposition_token`, but only when
    /// `scheduled.fields & WLR_XDG_POPUP_CONFIGURE_REPOSITION_TOKEN` is set.
    ///
    /// `None` on an ordinary configure. wlroots sends the
    /// `xdg_popup.repositioned` event itself off that field; **never forge
    /// one** — this accessor exists so a compositor can *observe* the token,
    /// for logging or for correlating its own state, not so it can echo it.
    #[must_use]
    pub fn reposition_token(&self) -> Option<u32> {
        // SAFETY: the handle's lifetime guarantees the popup is live;
        // `scheduled` is a plain embedded `wlr_xdg_popup_configure`.
        let scheduled: &sys::wlr_xdg_popup_configure = unsafe { &(*self.raw.as_ptr()).scheduled };
        let bit = sys::wlr_xdg_popup_configure_field::WLR_XDG_POPUP_CONFIGURE_REPOSITION_TOKEN.0;
        if scheduled.fields & bit == 0 {
            None
        } else {
            Some(scheduled.reposition_token)
        }
    }

    /// `wlr_xdg_popup_destroy` — sends `xdg_popup.popup_done`, destroys the role
    /// object and makes the resource inert.
    ///
    /// Does **not** touch the scene tree: the popup's subtree is a child of its
    /// parent's, and wlroots frees a tree's children recursively with the tree.
    /// Destroying it here would be the double free `Runtime::forget_toplevel`
    /// documents. Prefer
    /// [`Runtime::dismiss_popup`](crate::Runtime::dismiss_popup), which
    /// destroys a whole chain deepest-first — the order xdg-shell requires.
    pub fn destroy(&self) {
        // SAFETY: the handle's lifetime guarantees the popup is live. wlroots
        // emits `events.destroy` from inside this call, which runs
        // `on_popup_destroy`, which removes this popup's entry and listeners
        // before wlroots frees anything — the same ordering `on_toplevel_destroy`
        // relies on.
        unsafe { sys::wlr_xdg_popup_destroy(self.raw.as_ptr()) };
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

    /// Build a `wlr_xdg_positioner_rules` by hand. The struct is plain data —
    /// four `wlr_box`/size/offset groups, three enums and two bools, no
    /// pointers and no `wl_listener` — so `Default`-zeroing it and filling
    /// fields is sound, unlike the toplevel/decoration structs `runtime.rs`'s
    /// tests must `alloc_zeroed` behind a raw pointer.
    fn raw_rules(
        anchor_rect: (i32, i32, i32, i32),
        size: (i32, i32),
        anchor: u32,
        gravity: u32,
        adjustment: u32,
    ) -> sys::wlr_xdg_positioner_rules {
        let mut r: sys::wlr_xdg_positioner_rules = unsafe { std::mem::zeroed() };
        r.anchor_rect = sys::wlr_box {
            x: anchor_rect.0,
            y: anchor_rect.1,
            width: anchor_rect.2,
            height: anchor_rect.3,
        };
        r.size.width = size.0;
        r.size.height = size.1;
        r.anchor = sys::xdg_positioner_anchor(anchor);
        r.gravity = sys::xdg_positioner_gravity(gravity);
        r.constraint_adjustment = sys::xdg_positioner_constraint_adjustment(adjustment);
        r
    }

    /// The plain case: anchor the popup's top-left at the anchor rect's
    /// bottom-left and let it extend down-right. wlroots' own
    /// `wlr_xdg_positioner_rules_get_geometry` is what computes this — the
    /// crate deliberately does not reimplement the placement algebra, for the
    /// reason `geom.rs`'s module doc gives about `wlr_box` predicates: a
    /// reimplementation is free to drift, silently, in a patch release.
    #[test]
    fn the_geometry_anchors_and_gravitates_the_way_wlroots_does() {
        // anchor rect (10, 20, 100, 40); anchor = BOTTOM_LEFT (6);
        // gravity = BOTTOM_RIGHT (8); size 30x20.
        let rules =
            unsafe { PositionerRules::from_c(&raw_rules((10, 20, 100, 40), (30, 20), 6, 8, 0)) };
        let g = rules.geometry();
        assert_eq!((g.width, g.height), (30, 20), "the size is the client's");
        assert_eq!(
            (g.x, g.y),
            (10, 60),
            "bottom-left of (10,20,100,40) is (10,60), and BOTTOM_RIGHT gravity \
             puts the popup's top-left there"
        );
    }

    /// `unconstrain_box` is a pure function of the rules and a box: it touches
    /// no live popup, which is what makes it usable from a compositor deciding
    /// placement before anything is configured.
    #[test]
    fn unconstraining_slides_a_popup_back_inside_when_sliding_is_permitted() {
        // The popup would start at x = 190 and be 100 wide, running to 290 in a
        // 200-wide constraint. SLIDE_X (1) is permitted.
        let rules =
            unsafe { PositionerRules::from_c(&raw_rules((190, 0, 1, 1), (100, 20), 6, 8, 1)) };
        let free = rules.geometry();
        assert_eq!(
            free.x, 190,
            "unconstrained, it starts where it was asked to"
        );

        let fitted = rules.unconstrain_box(&Box2D::new(0, 0, 200, 200));
        assert!(
            fitted.x + fitted.width <= 200,
            "with SLIDE_X permitted the popup must end up inside the constraint; \
             got x={} width={}",
            fitted.x,
            fitted.width
        );
        assert_eq!(fitted.width, 100, "sliding must not resize");
    }

    /// Without a permitted adjustment there is nothing wlroots may do, and the
    /// answer is the unconstrained geometry — *not* a clamp this crate invents.
    /// A compositor that wants a clamp asks for one through the adjustment
    /// bits, which is the protocol's own design.
    #[test]
    fn unconstraining_with_no_adjustment_permitted_changes_nothing() {
        let rules =
            unsafe { PositionerRules::from_c(&raw_rules((190, 0, 1, 1), (100, 20), 6, 8, 0)) };
        assert_eq!(
            rules.unconstrain_box(&Box2D::new(0, 0, 200, 200)),
            rules.geometry()
        );
    }

    /// Untrusted input never panics (spec §7). Every anchor/gravity value a
    /// 32-bit client could possibly send — including the whole u32 range at the
    /// boundaries — is converted, round-tripped through C and asked for its
    /// geometry, and none of it may abort, panic or produce a NaN-shaped box.
    #[test]
    fn a_positioner_with_nonsense_enum_values_never_panics() {
        for raw in [
            0u32,
            8,
            9,
            10,
            255,
            1000,
            u32::MAX / 2,
            u32::MAX - 1,
            u32::MAX,
        ] {
            let rules = unsafe {
                PositionerRules::from_c(&raw_rules((0, 0, 1, 1), (10, 10), raw, raw, raw))
            };
            // Unknown anchors and gravities land on the protocol's initial
            // value; unknown *constraint* bits are kept verbatim, because that
            // mask goes straight back to wlroots (see `from_raw`'s doc).
            if raw > 8 {
                assert_eq!(rules.anchor, PositionerAnchor::None);
                assert_eq!(rules.gravity, PositionerGravity::None);
            }
            assert_eq!(rules.constraint_adjustment.bits(), raw);
            let _ = rules.geometry();
            let _ = rules.unconstrain_box(&Box2D::new(0, 0, 100, 100));
        }
    }

    /// Degenerate geometry from a client — zero and negative sizes, an empty
    /// anchor rect, an empty constraint — must come back as a value, never as
    /// an abort. `Box2D`'s own contract already calls a non-positive extent
    /// "empty", so an empty answer is a legitimate answer here.
    #[test]
    fn a_positioner_with_degenerate_sizes_never_panics() {
        for size in [(0, 0), (-1, -1), (i32::MIN, i32::MIN), (i32::MAX, i32::MAX)] {
            for rect in [(0, 0, 0, 0), (0, 0, -5, -5), (i32::MIN, i32::MIN, 1, 1)] {
                let rules = unsafe { PositionerRules::from_c(&raw_rules(rect, size, 6, 8, 63)) };
                let _ = rules.geometry();
                let _ = rules.unconstrain_box(&Box2D::default());
                let _ = rules.unconstrain_box(&Box2D::new(0, 0, 100, 100));
            }
        }
    }

    /// The copy is a *copy*: `to_c` then `from_c` must land back on the same
    /// value, or a rules snapshot handed to wlroots would not describe what the
    /// consumer read. This is also what keeps the `wlr_xdg_positioner_rules`
    /// coverage row honest.
    #[test]
    fn the_rules_round_trip_through_the_c_representation() {
        let mut raw = raw_rules((3, 4, 5, 6), (7, 8), 5, 3, 0b101010);
        raw.offset.x = -2;
        raw.offset.y = 9;
        raw.reactive = true;
        raw.has_parent_configure_serial = true;
        raw.parent_configure_serial = 4242;
        raw.parent_size.width = 800;
        raw.parent_size.height = 600;

        let rules = unsafe { PositionerRules::from_c(&raw) };
        assert_eq!(rules.anchor_rect, Box2D::new(3, 4, 5, 6));
        assert_eq!(rules.size, (7, 8));
        assert_eq!(rules.anchor, PositionerAnchor::TopLeft);
        assert_eq!(rules.gravity, PositionerGravity::Left);
        assert_eq!(rules.constraint_adjustment.bits(), 0b101010);
        assert_eq!(rules.offset, (-2, 9));
        assert!(rules.reactive);
        assert_eq!(rules.parent_configure_serial, Some(4242));
        assert_eq!(rules.parent_size, Some((800, 600)));

        let back = unsafe { PositionerRules::from_c(&rules.to_c()) };
        assert_eq!(back, rules);
    }

    /// `parent_size` and `parent_configure_serial` are `Option` on purpose: a
    /// client that never sent `set_parent_size`/`set_parent_configure` leaves
    /// zeroes there, and reporting `Some((0, 0))` would be indistinguishable
    /// from a client that really did send a zero size. wlroots flags the serial
    /// with `has_parent_configure_serial`; for the size, the zero *is* the
    /// sentinel wlroots itself uses.
    #[test]
    fn an_unsent_parent_size_and_serial_read_as_none() {
        let rules = unsafe { PositionerRules::from_c(&raw_rules((0, 0, 1, 1), (10, 10), 0, 0, 0)) };
        assert_eq!(rules.parent_size, None);
        assert_eq!(rules.parent_configure_serial, None);
    }

    /// Allocate a zeroed `wlr_xdg_popup` with a zeroed `wlr_xdg_surface` behind
    /// its `base`, and leak both.
    ///
    /// `alloc_zeroed` behind a raw pointer rather than `Box::new(zeroed())`,
    /// for the reason `runtime.rs`'s `record_toplevel_with_surface` documents:
    /// both structs embed `wl_listener`s whose bare function pointers are UB to
    /// *materialise* as a zero value, so the bytes are only ever touched
    /// through a pointer. Deliberately leaked — these tests never enter
    /// wlroots' own lifecycle, so nothing frees them, and a reclaimed
    /// allocation would leave a dangling `base` for any later assertion.
    fn scratch_popup(initialized: bool) -> *mut sys::wlr_xdg_popup {
        use std::alloc::{Layout, alloc_zeroed};

        // SAFETY: both layouts are non-zero-sized, so `alloc_zeroed` returns
        // either null (checked) or a suitably aligned zeroed allocation.
        let base = unsafe { alloc_zeroed(Layout::new::<sys::wlr_xdg_surface>()) }
            .cast::<sys::wlr_xdg_surface>();
        assert!(!base.is_null(), "allocation failed");
        let popup = unsafe { alloc_zeroed(Layout::new::<sys::wlr_xdg_popup>()) }
            .cast::<sys::wlr_xdg_popup>();
        assert!(!popup.is_null(), "allocation failed");
        // SAFETY: both allocations are freshly zeroed and exclusively owned;
        // every field written below is in bounds.
        unsafe {
            (*base).initialized = initialized;
            (*popup).base = base;
        }
        popup
    }

    /// A handle over a scratch popup with the given parent.
    ///
    /// # Safety
    ///
    /// The handle must not outlive `raw`; these tests leak `raw`, so it does
    /// not.
    unsafe fn handle(raw: *mut sys::wlr_xdg_popup, parent: PopupParent) -> Popup<'static> {
        unsafe { Popup::from_raw_with_id(raw, PopupId::dangling_for_test(), parent) }
    }

    #[test]
    fn a_handle_reports_the_parent_it_was_built_with_not_the_null_in_the_struct() {
        let raw = scratch_popup(false);
        let parent = PopupParent::Layer(LayerSurfaceId::dangling_for_test());
        // SAFETY: `raw` is leaked, so it outlives the handle.
        let popup = unsafe { handle(raw, parent) };
        assert_eq!(popup.parent(), parent);
        assert!(!popup.parent().is_popup());
        // `(*raw).parent` is the zeroed null a layer popup really does carry at
        // creation. If `parent()` ever started reading it, this test is the one
        // that notices.
        // SAFETY: `raw` is a live, exclusively-owned allocation.
        assert!(unsafe { (*raw).parent.is_null() });
    }

    /// `send_configure` must **skip the call** — not merely return zero — when
    /// the surface is not yet `initialized`. `wlr_xdg_surface_schedule_configure`
    /// contains `assert(surface->initialized)`, and this distribution ships
    /// wlroots without `NDEBUG`, so an early call is a hard `abort()` of the
    /// whole compositor process (the hazard `layer.rs`'s module doc documents
    /// in full for `wlr_layer_surface_v1_configure`). This test would *abort*
    /// rather than fail if the guard were removed, which is exactly the point:
    /// an aborting test binary is a louder failure than a red assertion.
    #[test]
    fn send_configure_is_skipped_entirely_before_the_surface_is_initialized() {
        let raw = scratch_popup(false);
        // SAFETY: `raw` is leaked, so it outlives the handle.
        let popup = unsafe { handle(raw, PopupParent::Toplevel(ToplevelId::dangling_for_test())) };
        assert_eq!(popup.send_configure(), 0);
    }

    /// `grab_requested` is `(*popup).seat != NULL` — wlroots fills that field
    /// only when the client sends `xdg_popup.grab`. It is the only observable
    /// this crate has for a popup grab: the export table has no
    /// `wlr_xdg_popup_grab_*` symbol at all.
    #[test]
    fn grab_requested_reads_the_seat_pointer_wlroots_fills_on_the_grab_request() {
        let raw = scratch_popup(false);
        // SAFETY: `raw` is leaked and exclusively owned.
        let popup = unsafe { handle(raw, PopupParent::Toplevel(ToplevelId::dangling_for_test())) };
        assert!(!popup.grab_requested(), "a zeroed seat is no grab");

        // SAFETY: writing a non-null sentinel into a field this crate only ever
        // null-checks. The pointer is never dereferenced by `grab_requested`.
        unsafe { (*raw).seat = std::ptr::dangling_mut() };
        assert!(popup.grab_requested());
    }

    /// The reposition token is present only when wlroots has set the
    /// `WLR_XDG_POPUP_CONFIGURE_REPOSITION_TOKEN` bit in `scheduled.fields`.
    /// Reading the token without checking the mask would report a stale token
    /// from a previous reposition on every ordinary configure.
    #[test]
    fn the_reposition_token_is_none_until_wlroots_sets_its_field_bit() {
        let raw = scratch_popup(false);
        // SAFETY: `raw` is leaked and exclusively owned.
        let popup = unsafe { handle(raw, PopupParent::Toplevel(ToplevelId::dangling_for_test())) };
        assert_eq!(popup.reposition_token(), None);

        // SAFETY: as above; both fields are plain integers in bounds.
        unsafe { (*raw).scheduled.reposition_token = 77 };
        assert_eq!(
            popup.reposition_token(),
            None,
            "a token with the field bit clear is stale, not current"
        );

        // SAFETY: as above.
        unsafe {
            (*raw).scheduled.fields =
                sys::wlr_xdg_popup_configure_field::WLR_XDG_POPUP_CONFIGURE_REPOSITION_TOKEN.0;
        }
        assert_eq!(popup.reposition_token(), Some(77));
    }

    /// `geometry` reads `current`, `anchor_rect`/`positioner_rules` read
    /// `scheduled.rules` — the distinction between "where it actually is" and
    /// "what the client last asked for", which a compositor deciding a
    /// reposition needs both halves of.
    #[test]
    fn geometry_reads_current_while_the_rules_read_what_was_scheduled() {
        let raw = scratch_popup(false);
        // SAFETY: `raw` is leaked and exclusively owned; every field written is
        // plain data in bounds.
        unsafe {
            (*raw).current.geometry = sys::wlr_box {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            };
            (*raw).current.reactive = true;
            (*raw).scheduled.rules.anchor_rect = sys::wlr_box {
                x: 10,
                y: 20,
                width: 30,
                height: 40,
            };
            (*raw).scheduled.rules.size.width = 64;
            (*raw).scheduled.rules.size.height = 48;
        }
        let popup = unsafe { handle(raw, PopupParent::Toplevel(ToplevelId::dangling_for_test())) };

        assert_eq!(popup.geometry(), Box2D::new(1, 2, 3, 4));
        assert!(popup.is_reactive());
        assert_eq!(popup.anchor_rect(), Box2D::new(10, 20, 30, 40));
        assert_eq!(popup.positioner_rules().size, (64, 48));
        assert_eq!(
            popup.positioner_rules().anchor_rect,
            Box2D::new(10, 20, 30, 40)
        );
    }

    /// The `Debug` impl is hand-written and prints named fields only. A derive
    /// would print the raw pointer, which is neither useful nor stable across
    /// runs — the reason `Toplevel`'s own `Debug` is hand-written.
    #[test]
    fn the_debug_impl_prints_named_fields_and_no_pointer() {
        let raw = scratch_popup(false);
        // SAFETY: `raw` is leaked and exclusively owned.
        let popup = unsafe { handle(raw, PopupParent::Toplevel(ToplevelId::dangling_for_test())) };
        let text = format!("{popup:?}");
        assert!(text.starts_with("Popup {"), "got {text}");
        assert!(text.contains("id: PopupId("), "got {text}");
        assert!(text.contains("parent: Toplevel("), "got {text}");
        assert!(
            !text.contains("0x"),
            "a raw pointer leaked into Debug: {text}"
        );
    }
}
