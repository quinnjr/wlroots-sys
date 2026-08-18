//! Boxes, floating-point boxes and output transforms.
//!
//! Three value types with no ownership anywhere. [`Box2D`] and [`FBox`] are
//! `#[repr(C)]` twins of `wlr_box` and `wlr_fbox`, pinned to those layouts at
//! compile time, so passing one to wlroots is a pointer cast rather than a
//! conversion.
//!
//! Every predicate here is an FFI call into wlroots rather than the two lines
//! of Rust it looks like. That is deliberate. `wlr/util/box.h` is full of edge
//! cases whose answers are not the obvious ones — containment is half-open,
//! `wlr_box_contains_box` is false when *either* box is empty, and
//! `wlr_box_closest_point` writes NaN for an empty box — and a reimplementation
//! would be free to drift from wlroots' answer on any of them, silently, in a
//! patch release. Calling C costs a few nanoseconds and cannot drift.
//!
//! `Box2D` is not called `Box`: shadowing `std::boxed::Box` in a crate whose
//! every wrapper allocates would be a poor trade for four characters.

use crate::sys;

/// An integer rectangle: `wlr_box`.
///
/// `x`/`y` are inclusive, `width`/`height` exclusive, so the box covers
/// `x..x + width`. Zero or negative extent means *empty*, which wlroots treats
/// interchangeably with "no box at all" — [`Default`] is therefore the empty
/// box, matching wlroots' own "leave it zeroed to mean unset" convention.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Box2D {
    /// Left edge, inclusive.
    pub x: i32,
    /// Top edge, inclusive.
    pub y: i32,
    /// Width; zero or negative means empty.
    pub width: i32,
    /// Height; zero or negative means empty.
    pub height: i32,
}

// `as_c` casts a `*const Box2D` straight to a `*const wlr_box`, which is sound
// only while the two have identical layouts. bindgen re-checks `wlr_box`
// against the header on every build, so this chain ends at the C declaration.
const _: () = assert!(size_of::<Box2D>() == size_of::<sys::wlr_box>());
const _: () = assert!(align_of::<Box2D>() == align_of::<sys::wlr_box>());
const _: () = assert!(std::mem::offset_of!(Box2D, x) == std::mem::offset_of!(sys::wlr_box, x));
const _: () = assert!(std::mem::offset_of!(Box2D, y) == std::mem::offset_of!(sys::wlr_box, y));
const _: () =
    assert!(std::mem::offset_of!(Box2D, width) == std::mem::offset_of!(sys::wlr_box, width));
const _: () =
    assert!(std::mem::offset_of!(Box2D, height) == std::mem::offset_of!(sys::wlr_box, height));

impl Box2D {
    /// Build a box from its four fields.
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Box2D {
        Box2D {
            x,
            y,
            width,
            height,
        }
    }

    /// This box as the C type, for the duration of the borrow.
    fn as_c(&self) -> *const sys::wlr_box {
        (&raw const *self).cast::<sys::wlr_box>()
    }

    /// True when the width and/or the height is zero or negative.
    pub fn empty(&self) -> bool {
        // SAFETY: `as_c` points at this live `Box2D`, whose layout is pinned
        // to `wlr_box` above; `wlr_box_empty` only reads it.
        unsafe { sys::wlr_box_empty(self.as_c()) }
    }

    /// True when `(x, y)` lies inside this box.
    ///
    /// Half-open, following wlroots exactly: `(100, 50)` is **not** in
    /// `(0, 0, 100, 50)`, while `(10, 10)` is in `(10, 0, 50, 50)`. An empty
    /// box contains nothing.
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        // SAFETY: as above; `wlr_box_contains_point` only reads the box.
        unsafe { sys::wlr_box_contains_point(self.as_c(), x, y) }
    }

    /// True when `smaller` lies wholly inside this box.
    ///
    /// **False if either box is empty**, which is wlroots' rule and not the one
    /// set-theory would give (an empty box is contained in everything).
    pub fn contains_box(&self, smaller: &Box2D) -> bool {
        // SAFETY: both pointers target live `Box2D`s with `wlr_box`'s layout;
        // `wlr_box_contains_box` only reads them.
        unsafe { sys::wlr_box_contains_box(self.as_c(), smaller.as_c()) }
    }

    /// The point inside this box nearest to `(x, y)`.
    ///
    /// `None` when there is no such point — an empty box, or a NaN input.
    /// wlroots writes NaN into its out-parameters in that case; handing a
    /// consumer a NaN that poisons every later arithmetic operation would be a
    /// worse answer than saying there is none.
    pub fn closest_point(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let mut dest_x = 0.0;
        let mut dest_y = 0.0;
        // SAFETY: `as_c` points at this live box; the two out-parameters point
        // at live locals that outlive the call.
        unsafe {
            sys::wlr_box_closest_point(self.as_c(), x, y, &raw mut dest_x, &raw mut dest_y);
        }
        if dest_x.is_nan() || dest_y.is_nan() {
            return None;
        }
        Some((dest_x, dest_y))
    }

    /// The overlap between this box and `other`, or `None` when they are
    /// disjoint (or either is empty).
    pub fn intersection(&self, other: &Box2D) -> Option<Box2D> {
        let mut dest = Box2D::default();
        // SAFETY: `dest` is a live, exclusively-borrowed local with `wlr_box`'s
        // layout, and the two sources are live and only read. wlroots writes an
        // empty box into `dest` on the false path, so `dest` is initialised
        // either way.
        let intersects = unsafe {
            sys::wlr_box_intersection(
                (&raw mut dest).cast::<sys::wlr_box>(),
                self.as_c(),
                other.as_c(),
            )
        };
        intersects.then_some(dest)
    }

    /// This box transformed within a `(0, 0, width, height)` frame.
    pub fn transformed(&self, transform: Transform, width: i32, height: i32) -> Box2D {
        let mut dest = Box2D::default();
        // SAFETY: `dest` is a live, exclusively-borrowed local; `self` is only
        // read. wlroots documents `dest` may alias the source, so the distinct
        // allocation used here is the conservative case.
        unsafe {
            sys::wlr_box_transform(
                (&raw mut dest).cast::<sys::wlr_box>(),
                self.as_c(),
                transform.into(),
                width,
                height,
            );
        }
        dest
    }

    /// wlroots' notion of equality, which is **not** field-wise: *any two empty
    /// boxes are equal*, however differently they are spelled.
    ///
    /// `Box2D::new(0, 0, 0, 0).equals(&Box2D::new(5, 5, -1, 3))` is true, while
    /// the two are `!=` under [`PartialEq`]. Both answers are right for their
    /// own question — the empty box has no position, so nothing downstream of
    /// "is this the same rectangle on screen?" should distinguish them, but a
    /// consumer diffing stored state wants the fields.
    ///
    /// Use this when comparing against something wlroots produced (an output's
    /// current geometry, a damage extent); use `==` when comparing values this
    /// crate's own consumer stored. `tests/geom.rs` pins the two apart, since a
    /// C-side change either way would otherwise be invisible.
    pub fn equals(&self, other: &Box2D) -> bool {
        // SAFETY: both pointers target live boxes and are only read.
        unsafe { sys::wlr_box_equal(self.as_c(), other.as_c()) }
    }
}

impl From<(i32, i32, i32, i32)> for Box2D {
    /// `(x, y, width, height)`, the order every tuple in this crate's older
    /// signatures (`Runtime::output_layout_box`) already uses.
    fn from((x, y, width, height): (i32, i32, i32, i32)) -> Box2D {
        Box2D::new(x, y, width, height)
    }
}

impl From<Box2D> for (i32, i32, i32, i32) {
    fn from(b: Box2D) -> (i32, i32, i32, i32) {
        (b.x, b.y, b.width, b.height)
    }
}

/// A floating-point rectangle: `wlr_fbox`.
///
/// Used where wlroots works in surface-local coordinates that are not whole
/// pixels — a render pass's source box, most of all.
///
/// No `Eq`/`Hash`: the fields are `f64`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FBox {
    /// Left edge, inclusive.
    pub x: f64,
    /// Top edge, inclusive.
    pub y: f64,
    /// Width; zero or negative means empty.
    pub width: f64,
    /// Height; zero or negative means empty.
    pub height: f64,
}

const _: () = assert!(size_of::<FBox>() == size_of::<sys::wlr_fbox>());
const _: () = assert!(align_of::<FBox>() == align_of::<sys::wlr_fbox>());
const _: () = assert!(std::mem::offset_of!(FBox, x) == std::mem::offset_of!(sys::wlr_fbox, x));
const _: () = assert!(std::mem::offset_of!(FBox, y) == std::mem::offset_of!(sys::wlr_fbox, y));
const _: () =
    assert!(std::mem::offset_of!(FBox, width) == std::mem::offset_of!(sys::wlr_fbox, width));
const _: () =
    assert!(std::mem::offset_of!(FBox, height) == std::mem::offset_of!(sys::wlr_fbox, height));

impl FBox {
    /// Build a box from its four fields.
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> FBox {
        FBox {
            x,
            y,
            width,
            height,
        }
    }

    /// This box as the C type, for the duration of the borrow.
    ///
    /// `pub(crate)` because the scene's source-box setter hands one straight
    /// to wlroots — the cast is sound only because of the layout assertions
    /// above, so it stays in this module's gift rather than becoming a public
    /// escape hatch.
    pub(crate) fn as_c(&self) -> *const sys::wlr_fbox {
        (&raw const *self).cast::<sys::wlr_fbox>()
    }

    /// True when the width and/or the height is zero or negative.
    pub fn empty(&self) -> bool {
        // SAFETY: `as_c` points at this live `FBox`, whose layout is pinned to
        // `wlr_fbox` above; `wlr_fbox_empty` only reads it.
        unsafe { sys::wlr_fbox_empty(self.as_c()) }
    }

    /// This box transformed within a `(0, 0, width, height)` frame.
    pub fn transformed(&self, transform: Transform, width: f64, height: f64) -> FBox {
        let mut dest = FBox::default();
        // SAFETY: `dest` is a live, exclusively-borrowed local with
        // `wlr_fbox`'s layout; `self` is only read.
        unsafe {
            sys::wlr_fbox_transform(
                (&raw mut dest).cast::<sys::wlr_fbox>(),
                self.as_c(),
                transform.into(),
                width,
                height,
            );
        }
        dest
    }

    /// wlroots' notion of equality; see [`Box2D::equals`], including the rule
    /// that any two empty boxes are equal to each other.
    pub fn equals(&self, other: &FBox) -> bool {
        // SAFETY: both pointers target live boxes and are only read.
        unsafe { sys::wlr_fbox_equal(self.as_c(), other.as_c()) }
    }
}

/// How a buffer's contents are rotated and/or flipped when displayed:
/// `enum wl_output_transform`.
///
/// The eight values are Wayland protocol ABI, fixed since `wl_output` version
/// 1, and are pinned against libwayland's own constants by this module's tests
/// rather than by this comment.
///
/// `#[non_exhaustive]` even though the protocol set is closed: this enum is
/// also how the crate will spell a transform for the rest of the 0.20 line,
/// during which the hand-written API is frozen (see `CLAUDE.md`), and a
/// wayland-protocols addition would otherwise have nowhere to go until 0.21.
///
/// Conversions to and from [`sys::wl_output_transform`] are lossless in both
/// directions, so a consumer holding a `wayland-server` transform is never
/// forced to launder it through this type.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Transform {
    /// No transform.
    #[default]
    Normal = 0,
    /// 90° counter-clockwise.
    R90 = 1,
    /// 180°.
    R180 = 2,
    /// 270° counter-clockwise.
    R270 = 3,
    /// Flipped horizontally.
    Flipped = 4,
    /// Flipped, then rotated 90° counter-clockwise.
    Flipped90 = 5,
    /// Flipped, then rotated 180°.
    Flipped180 = 6,
    /// Flipped, then rotated 270° counter-clockwise.
    Flipped270 = 7,
}

impl Transform {
    /// Decode a raw `wl_output_transform` value.
    ///
    /// `None` for anything outside 0..=7 rather than a panic: the value can
    /// come from a client, and this crate's rule is that nothing reachable from
    /// a handler may abort the process.
    pub fn from_raw(value: u32) -> Option<Transform> {
        Some(match value {
            0 => Transform::Normal,
            1 => Transform::R90,
            2 => Transform::R180,
            3 => Transform::R270,
            4 => Transform::Flipped,
            5 => Transform::Flipped90,
            6 => Transform::Flipped180,
            7 => Transform::Flipped270,
            _ => return None,
        })
    }

    /// The raw `wl_output_transform` value.
    pub fn to_raw(self) -> u32 {
        self as u32
    }

    /// The transform that composes with this one to give [`Transform::Normal`].
    pub fn invert(self) -> Transform {
        // SAFETY: a pure function on a scalar; no pointers are involved.
        let raw = unsafe { sys::wlr_output_transform_invert(self.into()) };
        // wlroots maps the eight valid inputs onto the same eight outputs, so
        // the fallback is unreachable; it exists because this must not panic.
        Transform::from_raw(raw.0).unwrap_or(Transform::Normal)
    }

    /// The single transform equivalent to applying `self` and then `then`.
    ///
    /// **Not commutative** once a flip is involved: `Flipped.compose(R90)` and
    /// `R90.compose(Flipped)` differ, which this module's tests pin.
    pub fn compose(self, then: Transform) -> Transform {
        // SAFETY: a pure function on two scalars.
        let raw = unsafe { sys::wlr_output_transform_compose(self.into(), then.into()) };
        Transform::from_raw(raw.0).unwrap_or(Transform::Normal)
    }

    /// Swap `x` and `y` when this transform turns the axes.
    ///
    /// Despite the C name (`wlr_output_transform_coords`) this is **not** a
    /// point transform: it does nothing at all for `Flipped` or `R180`, and for
    /// `R90` it swaps the pair rather than rotating it. What it actually
    /// answers is "does this transform exchange the two axes?", which is the
    /// question a caller converting a *size* — an output's width and height, a
    /// buffer's extents — needs. Verified against wlroots 0.20 by exercising
    /// all eight transforms (`tests/geom.rs`); the crate reports what wlroots
    /// does rather than what the name suggests.
    ///
    /// For a box, use [`Box2D::transformed`], which is a real transform within
    /// a frame.
    pub fn apply_coords(self, x: i32, y: i32) -> (i32, i32) {
        let mut x = x;
        let mut y = y;
        // SAFETY: both out-parameters point at live locals that outlive the
        // call, and wlroots writes to them in place.
        unsafe {
            sys::wlr_output_transform_coords(self.into(), &raw mut x, &raw mut y);
        }
        (x, y)
    }
}

impl From<Transform> for sys::wl_output_transform {
    fn from(t: Transform) -> sys::wl_output_transform {
        sys::wl_output_transform(t as u32)
    }
}

impl TryFrom<sys::wl_output_transform> for Transform {
    type Error = ();

    /// Fails for a value outside 0..=7. The error is `()` because there is
    /// nothing to say about it that the input does not already say.
    fn try_from(raw: sys::wl_output_transform) -> Result<Transform, ()> {
        Transform::from_raw(raw.0).ok_or(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eight discriminants are Wayland ABI. Pin them against libwayland's
    /// own constants rather than against the comment next to them.
    #[test]
    fn transform_values_match_the_wayland_protocol() {
        for (ours, theirs) in [
            (
                Transform::Normal,
                sys::wl_output_transform::WL_OUTPUT_TRANSFORM_NORMAL,
            ),
            (
                Transform::R90,
                sys::wl_output_transform::WL_OUTPUT_TRANSFORM_90,
            ),
            (
                Transform::R180,
                sys::wl_output_transform::WL_OUTPUT_TRANSFORM_180,
            ),
            (
                Transform::R270,
                sys::wl_output_transform::WL_OUTPUT_TRANSFORM_270,
            ),
            (
                Transform::Flipped,
                sys::wl_output_transform::WL_OUTPUT_TRANSFORM_FLIPPED,
            ),
            (
                Transform::Flipped90,
                sys::wl_output_transform::WL_OUTPUT_TRANSFORM_FLIPPED_90,
            ),
            (
                Transform::Flipped180,
                sys::wl_output_transform::WL_OUTPUT_TRANSFORM_FLIPPED_180,
            ),
            (
                Transform::Flipped270,
                sys::wl_output_transform::WL_OUTPUT_TRANSFORM_FLIPPED_270,
            ),
        ] {
            assert_eq!(ours.to_raw(), theirs.0, "{ours:?}");
            assert_eq!(Transform::try_from(theirs), Ok(ours));
        }
    }

    #[test]
    fn from_raw_rejects_everything_outside_the_protocol_range() {
        assert_eq!(Transform::from_raw(8), None);
        assert_eq!(Transform::from_raw(u32::MAX), None);
    }
}
