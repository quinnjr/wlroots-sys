//! Pixel regions — sets of rectangles, as pixman spells them.
//!
//! wlroots uses `pixman_region32_t` everywhere a set of rectangles is needed:
//! surface damage, a render pass's clip, `wlr_damage_ring`'s accumulated
//! damage. Wrapping any of that means being able to build and read one.
//!
//! # Why the pixman functions are declared here
//!
//! `wlr-sys`' bindgen allowlist is `wlr_.*`, so pixman *types*
//! (`pixman_region32`, `pixman_box32`) are bound — bindgen pulls in whatever
//! wlroots' own structs reference — but pixman *functions* are not. Rather than
//! widen the allowlist (a `wlr-sys` release, enlarging a frozen surface for the
//! whole 0.20 line) or take a `pixman` crate dependency (which would introduce a
//! *second*, non-identical `pixman_region32` type — precisely the failure mode
//! `wlr-sys`' `tests/interop.rs` exists to prevent), the dozen-odd entry points
//! this module needs are declared below against the `wlr-sys` types. Type
//! identity is preserved because the type still comes from `wlr-sys`; only the
//! declaration is local. `crates/wlr/build.rs` probes `pixman-1` to put
//! `-lpixman-1` on the link line, which wlroots' own `Requires.private` keeps
//! off it.
//!
//! # No raw region in the public API
//!
//! Nothing here hands out a `*mut pixman_region32`. A clip or damage parameter
//! is `Option<&Region>`; a region wlroots owns is a [`RegionRef`]. An escape
//! hatch returning the raw pointer would be coverage in name only.
//!
//! # Allocation failure
//!
//! Every pixman operation that can grow a region returns a `pixman_bool_t` that
//! is false only on malloc failure, and leaves the destination as the shared
//! empty region when it does. Rust's own allocator aborts rather than reporting
//! OOM, so surfacing that as a `Result` on every method would add a branch no
//! consumer could act on differently; these methods return the (empty) region
//! pixman produced, and say so here rather than in nine doc comments.

use std::ffi::{c_int, c_uint};
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::geom::{Box2D, Transform};
use crate::sys;

// The pixman entry points this module needs, declared against `wlr-sys`' own
// `pixman_region32` / `pixman_box32` so the types stay identical to the ones in
// wlroots' signatures. `pixman_bool_t` is a plain `int`.
//
// Keep this list minimal: every declaration here is an unchecked assertion
// about pixman's ABI, and pixman's headers are the only thing that validates
// it. The signatures below were read from `/usr/include/pixman-1/pixman.h`.
unsafe extern "C" {
    fn pixman_region32_init(region: *mut sys::pixman_region32);
    fn pixman_region32_init_rect(
        region: *mut sys::pixman_region32,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
    );
    fn pixman_region32_init_rects(
        region: *mut sys::pixman_region32,
        boxes: *const sys::pixman_box32,
        count: c_int,
    ) -> c_int;
    fn pixman_region32_fini(region: *mut sys::pixman_region32);
    fn pixman_region32_copy(
        dest: *mut sys::pixman_region32,
        source: *const sys::pixman_region32,
    ) -> c_int;
    fn pixman_region32_union(
        new_reg: *mut sys::pixman_region32,
        reg1: *const sys::pixman_region32,
        reg2: *const sys::pixman_region32,
    ) -> c_int;
    fn pixman_region32_intersect(
        new_reg: *mut sys::pixman_region32,
        reg1: *const sys::pixman_region32,
        reg2: *const sys::pixman_region32,
    ) -> c_int;
    fn pixman_region32_subtract(
        reg_d: *mut sys::pixman_region32,
        reg_m: *const sys::pixman_region32,
        reg_s: *const sys::pixman_region32,
    ) -> c_int;
    fn pixman_region32_translate(region: *mut sys::pixman_region32, x: c_int, y: c_int);
    fn pixman_region32_not_empty(region: *const sys::pixman_region32) -> c_int;
    fn pixman_region32_extents(region: *const sys::pixman_region32) -> *mut sys::pixman_box32;
    fn pixman_region32_rectangles(
        region: *const sys::pixman_region32,
        n_rects: *mut c_int,
    ) -> *mut sys::pixman_box32;
    fn pixman_region32_contains_point(
        region: *const sys::pixman_region32,
        x: c_int,
        y: c_int,
        box_: *mut sys::pixman_box32,
    ) -> c_int;
    fn pixman_region32_equal(
        region1: *const sys::pixman_region32,
        region2: *const sys::pixman_region32,
    ) -> c_int;
}

/// `pixman_box32` is `{x1, y1, x2, y2}` — corners, not origin-and-extent —
/// which is the one conversion in this module a reader will get wrong.
///
/// The extent is saturated because the difference of two `i32` corners does
/// not always fit in an `i32`: a region spanning most of the coordinate space
/// — which [`Region::expanded`] can legitimately produce — has a width above
/// `i32::MAX`. Subtracting directly panicked in debug builds and wrapped to a
/// negative extent in release ones, so a caller asking a very wide region for
/// its extents got either a crash or a box claiming negative size.
fn box_from_pixman(b: sys::pixman_box32) -> Box2D {
    Box2D {
        x: b.x1,
        y: b.y1,
        width: b.x2.saturating_sub(b.x1),
        height: b.y2.saturating_sub(b.y1),
    }
}

/// Whether a scale factor is one pixman can act on: finite and above zero.
///
/// Anything else makes wlroots hand pixman an inverted or non-finite
/// rectangle, which it refuses loudly on stderr rather than by return value.
fn usable_scale(scale: f32) -> bool {
    scale.is_finite() && scale > 0.0
}

/// The reverse, clamping so a negative extent becomes an empty corner pair
/// rather than an inverted box pixman would misbehave on.
///
/// A box whose far corner does not fit in an `i32` becomes empty rather than
/// truncated. `saturating_add` alone was worse than it looks: a box at
/// `x = i32::MAX - 1` with `width = 10` came back one pixel wide, silently,
/// while the same box through [`Region::from_box`] made pixman print
/// `*** BUG *** In pixman_region32_init_rect` and yield nothing. Two
/// constructors gave two different wrong answers for one input; now both
/// treat an unrepresentable box as contributing nothing.
fn box_to_pixman(b: Box2D) -> sys::pixman_box32 {
    let w = b.width.max(0);
    let h = b.height.max(0);
    let (x2, x_ok) = b.x.overflowing_add(w);
    let (y2, y_ok) = b.y.overflowing_add(h);
    if x_ok || y_ok {
        // Empty: pixman treats x1 == x2 as no area at all.
        return sys::pixman_box32 {
            x1: b.x,
            y1: b.y,
            x2: b.x,
            y2: b.y,
        };
    }
    sys::pixman_box32 {
        x1: b.x,
        y1: b.y,
        x2,
        y2,
    }
}

/// The read-only surface, shared by [`Region`] and [`RegionRef`] so neither can
/// drift from the other.
///
/// # Safety
///
/// Every function here requires `p` to point at a live, initialised
/// `pixman_region32`.
mod read {
    use super::{Box2D, box_from_pixman, sys};
    use std::ffi::c_int;

    pub(super) unsafe fn is_empty(p: *const sys::pixman_region32) -> bool {
        // SAFETY: the caller guarantees `p` is live and initialised;
        // `pixman_region32_not_empty` only reads it.
        unsafe { super::pixman_region32_not_empty(p) == 0 }
    }

    pub(super) unsafe fn extents(p: *const sys::pixman_region32) -> Box2D {
        // SAFETY: as above. `pixman_region32_extents` returns a pointer into
        // the region itself (or into pixman's shared empty region), valid for
        // as long as `p` is and never null.
        unsafe { box_from_pixman(*super::pixman_region32_extents(p)) }
    }

    pub(super) unsafe fn contains_point(p: *const sys::pixman_region32, x: i32, y: i32) -> bool {
        // SAFETY: as above. The out-box is optional and left null.
        unsafe { super::pixman_region32_contains_point(p, x, y, std::ptr::null_mut()) != 0 }
    }

    pub(super) unsafe fn rectangles(
        p: *const sys::pixman_region32,
    ) -> &'static [sys::pixman_box32] {
        let mut n: c_int = 0;
        // SAFETY: as above; `n_rects` points at a live local.
        let first = unsafe { super::pixman_region32_rectangles(p, &raw mut n) };
        if n <= 0 || first.is_null() {
            return &[];
        }
        // SAFETY: pixman returns `n` contiguous boxes owned by the region, so
        // the slice is valid for as long as the region is unmodified. The
        // `'static` here is a lie the two callers immediately re-bind to their
        // own borrow — neither exposes it.
        unsafe { std::slice::from_raw_parts(first, n as usize) }
    }
}

/// An owned set of rectangles.
///
/// Boxed rather than held inline: a `Region` is handed to C as a pointer, and
/// a stable address means moving the `Region` value cannot turn one of those
/// into a dangling-pointer bug that happens to work.
///
/// This used to name a render pass's clip as the example, and that was wrong
/// in the one direction that matters — it claimed wlroots keeps the pointer
/// past the producing statement, when the Vulkan backend
/// `pixman_region32_copy`s the clip on the way in and refs the timeline
/// (`vulkan/pass.c`). `RenderPass`'s own SAFETY comment says so, and
/// `examples/render_pass.rs` relies on it by dropping its clip before submit.
/// A reader who trusted this comment over that one would have concluded the
/// example was broken. The `Box` is still right — a stable address costs
/// nothing and the property is easy to lose — it just is not load-bearing for
/// the reason stated here.
///
/// `!Send` and `!Sync`, like everything in this crate that owns a C allocation.
pub struct Region {
    inner: Box<sys::pixman_region32>,
}

impl Region {
    /// A fresh empty region.
    pub fn new() -> Region {
        let mut inner = Region::uninit();
        // SAFETY: `inner` is a fresh, uniquely-owned allocation at a stable
        // address, and `pixman_region32_init` initialises it in place.
        unsafe { pixman_region32_init(&raw mut *inner) };
        Region { inner }
    }

    /// A region covering exactly `b`. An empty box gives an empty region.
    pub fn from_box(b: Box2D) -> Region {
        let mut inner = Region::uninit();
        // SAFETY: as in `new`. Negative extents are clamped to zero first
        // because pixman takes the width and height as `unsigned`, where a
        // negative would become an enormous positive.
        unsafe {
            pixman_region32_init_rect(
                &raw mut *inner,
                b.x,
                b.y,
                b.width.max(0) as c_uint,
                b.height.max(0) as c_uint,
            );
        }
        Region { inner }
    }

    /// A region covering the union of `boxes`.
    ///
    /// pixman does the merging; overlapping or unsorted input is fine, and the
    /// result is the same canonical rectangle decomposition every other
    /// operation produces. That is why [`rectangles`](Self::rectangles) does
    /// not in general give back what was passed in.
    pub fn from_boxes(boxes: &[Box2D]) -> Region {
        let converted: Vec<sys::pixman_box32> = boxes.iter().copied().map(box_to_pixman).collect();
        let mut inner = Region::uninit();
        // SAFETY: as in `new`; `converted` is a live slice that outlives the
        // call, and its length is what is passed as the count. A count beyond
        // `c_int` is not representable by any slice pixman could accept, so the
        // saturating conversion cannot silently truncate a real input.
        unsafe {
            pixman_region32_init_rects(
                &raw mut *inner,
                converted.as_ptr(),
                c_int::try_from(converted.len()).unwrap_or(c_int::MAX),
            );
        }
        Region { inner }
    }

    /// An allocation for a `pixman_region32` that has *not* been initialised.
    ///
    /// Zeroed rather than `MaybeUninit` because every caller hands it straight
    /// to a `pixman_region32_init*`, which overwrites it — and because a zeroed
    /// `{pixman_box32, *mut data}` is a valid Rust value, so no `MaybeUninit`
    /// dance is needed to say so.
    fn uninit() -> Box<sys::pixman_region32> {
        // SAFETY: `pixman_region32` is four `i32`s and a pointer; all-zero is a
        // valid bit pattern for it.
        Box::new(unsafe { std::mem::zeroed::<sys::pixman_region32>() })
    }

    pub(crate) fn as_ptr(&self) -> *const sys::pixman_region32 {
        &raw const *self.inner
    }

    /// The mutable form, for the calls that write into a destination region.
    ///
    /// Separate from [`as_ptr`](Self::as_ptr) rather than a `cast_mut()` on it:
    /// a pointer derived from a shared borrow must not be written through, and
    /// that is the kind of provenance bug that runs correctly for years.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut sys::pixman_region32 {
        &raw mut *self.inner
    }

    /// True when the region covers nothing.
    pub fn is_empty(&self) -> bool {
        // SAFETY: `self.inner` is live and was initialised by a constructor.
        unsafe { read::is_empty(self.as_ptr()) }
    }

    /// The smallest box containing every rectangle. Empty for an empty region.
    pub fn extents(&self) -> Box2D {
        // SAFETY: as above.
        unsafe { read::extents(self.as_ptr()) }
    }

    /// True when `(x, y)` is covered.
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        // SAFETY: as above.
        unsafe { read::contains_point(self.as_ptr(), x, y) }
    }

    /// The rectangles making up the region, in pixman's canonical
    /// decomposition (band-sorted, non-overlapping) — not the boxes it was
    /// built from.
    pub fn rectangles(&self) -> impl Iterator<Item = Box2D> + use<'_> {
        // SAFETY: `self.inner` is live and initialised; the borrow of `self`
        // outlives the returned iterator, and nothing in that iterator can
        // modify the region, so the slice stays valid for its whole life.
        unsafe { read::rectangles(self.as_ptr()) }
            .iter()
            .copied()
            .map(box_from_pixman)
    }

    /// Everything covered by either region.
    pub fn union(&self, other: &Region) -> Region {
        self.binary_op(other, pixman_region32_union)
    }

    /// Everything covered by both regions.
    pub fn intersect(&self, other: &Region) -> Region {
        self.binary_op(other, pixman_region32_intersect)
    }

    /// Everything covered by `self` but not by `other`.
    pub fn subtract(&self, other: &Region) -> Region {
        self.binary_op(other, pixman_region32_subtract)
    }

    /// Shared body of the three set operations: a fresh destination, never an
    /// alias of either source.
    fn binary_op(
        &self,
        other: &Region,
        op: unsafe extern "C" fn(
            *mut sys::pixman_region32,
            *const sys::pixman_region32,
            *const sys::pixman_region32,
        ) -> c_int,
    ) -> Region {
        let mut out = Region::new();
        // SAFETY: `out` is freshly initialised and distinct from both sources,
        // which are live and only read. The `pixman_bool_t` is discarded per
        // this module's docs: false means malloc failed and `out` was left
        // empty, which is what is returned.
        unsafe { op(out.as_mut_ptr(), self.as_ptr(), other.as_ptr()) };
        out
    }

    /// This region moved by `(dx, dy)`.
    pub fn translated(&self, dx: i32, dy: i32) -> Region {
        let mut out = self.clone();
        // SAFETY: `out` is a live region this call exclusively owns;
        // `pixman_region32_translate` mutates it in place.
        unsafe { pixman_region32_translate(out.as_mut_ptr(), dx, dy) };
        out
    }

    /// This region scaled by `scale`, rounded **outward**: for `scale >= 1` the
    /// result contains the scaled original rather than merely approximating it.
    ///
    /// An empty region for a `scale` that is not finite and positive. wlroots
    /// multiplies the corners and hands the result to pixman, which rejects an
    /// inverted or non-finite rectangle by printing
    /// `*** BUG *** In pixman_region32_init_rect` to the process's stderr and
    /// returning nothing — so `0.0`, a negative, `NAN` and `INFINITY` all
    /// produced an empty region *plus* a line of libpixman noise in a
    /// compositor's log that no consumer could act on. The answer is the same;
    /// only the noise is gone.
    pub fn scaled(&self, scale: f32) -> Region {
        let mut out = Region::new();
        if !usable_scale(scale) {
            return out;
        }
        // Calls `wlr_region_scale` rather than delegating to `scaled_xy`:
        // wlroots exposes both, the coverage ledger records this crate as
        // wrapping both, and a delegation would make that record false — the
        // audit's `no_wrapped_entry_is_stale` check catches exactly that, and
        // did when this was first written as a delegation.
        //
        // SAFETY: `out` is initialised and distinct from `self`; wlroots does
        // not document `dst == src` as supported, so it is never aliased here.
        unsafe { sys::wlr_region_scale(out.as_mut_ptr(), self.as_ptr(), scale) };
        out
    }

    /// [`scaled`](Self::scaled) with a separate factor per axis.
    ///
    /// Empty for any factor that is not finite and positive, as
    /// [`scaled`](Self::scaled) describes.
    pub fn scaled_xy(&self, scale_x: f32, scale_y: f32) -> Region {
        let mut out = Region::new();
        if !usable_scale(scale_x) || !usable_scale(scale_y) {
            return out;
        }
        // SAFETY: `out` is initialised and distinct from `self`; wlroots does
        // not document `dst == src` as supported, so it is never aliased here.
        unsafe {
            sys::wlr_region_scale_xy(out.as_mut_ptr(), self.as_ptr(), scale_x, scale_y);
        }
        out
    }

    /// This region transformed within a `(0, 0, width, height)` frame.
    pub fn transformed(&self, transform: Transform, width: i32, height: i32) -> Region {
        let mut out = Region::new();
        // SAFETY: as in `scaled`.
        unsafe {
            sys::wlr_region_transform(
                out.as_mut_ptr(),
                self.as_ptr(),
                transform.into(),
                width,
                height,
            );
        }
        out
    }

    /// This region grown by `distance` on both axes.
    ///
    /// `u32` rather than `i32` because wlroots requires a non-negative
    /// distance, and an unrepresentable precondition needs no runtime check.
    ///
    /// A distance larger than this region can absorb is reduced to the largest
    /// one that can be. Clamping the *argument* to `i32::MAX` was not enough
    /// and read as though it were: wlroots computes `x2 + distance`, so
    /// `expanded(u32::MAX)` on a small box overflowed and came back with
    /// extents near `i32::MIN` and a width of 8 — a smaller region, from a
    /// call asking to grow it. The bound has to come from this region's own
    /// extents, which is what it does now.
    pub fn expanded(&self, distance: u32) -> Region {
        let mut out = Region::new();
        let capped = self.max_expansion().min(distance);
        // SAFETY: as in `scaled`. `capped` is non-negative and small enough
        // that neither `x1 - capped` nor `x2 + capped` can leave `i32`, which
        // is the arithmetic `wlr_region_expand` performs on every rectangle.
        unsafe {
            sys::wlr_region_expand(
                out.as_mut_ptr(),
                self.as_ptr(),
                c_int::try_from(capped).unwrap_or(c_int::MAX),
            );
        }
        out
    }

    /// The largest distance [`expanded`](Self::expanded) can apply to this
    /// region without any corner leaving `i32`.
    ///
    /// Read from the raw pixman corners, **not** from
    /// [`extents`](Self::extents). That returns a [`Box2D`], whose `width` is
    /// `x2 - x1` through `saturating_sub`, and the saturation loses exactly
    /// the information this needs: a region spanning `x1 = -2, x2 = i32::MAX`
    /// reports `width = i32::MAX`, so reconstructing the right edge as
    /// `x + width` saturates back to `i32::MAX - 2` and claims two pixels of
    /// headroom at an edge that has none — and `wlr_region_expand` overflows
    /// anyway. The corners pixman actually holds are the only honest source.
    fn max_expansion(&self) -> u32 {
        let head_room = |v: i32| -> u32 {
            // Distance to whichever bound this coordinate is nearer.
            let to_max = (i32::MAX as i64 - v as i64) as u64;
            let to_min = (v as i64 - i32::MIN as i64) as u64;
            to_max.min(to_min).min(u32::MAX as u64) as u32
        };
        // SAFETY: as in `extents`. `pixman_region32_extents` returns a pointer
        // into the region itself (or pixman's shared empty region), valid for
        // as long as `self` is and never null.
        let b = unsafe { *pixman_region32_extents(self.as_ptr()) };
        head_room(b.x1)
            .min(head_room(b.y1))
            .min(head_room(b.x2))
            .min(head_room(b.y2))
    }

    /// The smallest region containing this one rotated by `rotation` **radians**
    /// about `(ox, oy)`.
    ///
    /// wlroots' header does not say which unit it takes. Radians is what its
    /// implementation uses, and this module's tests pin it: rotating a wide,
    /// short region by `PI` about the origin lands it in the opposite quadrant,
    /// which a 3.14-*degree* rotation could not do.
    pub fn rotated_bounds(&self, rotation: f32, ox: i32, oy: i32) -> Region {
        let mut out = Region::new();
        // SAFETY: as in `scaled`.
        unsafe {
            sys::wlr_region_rotated_bounds(out.as_mut_ptr(), self.as_ptr(), rotation, ox, oy);
        }
        out
    }

    /// Confine a movement to this region.
    ///
    /// Given an old position `from` that is inside the region and a tentative
    /// new position `to`, returns the confined new position. `None` when `from`
    /// is not inside the region — the case a pointer-constraint implementation
    /// has to treat as "the constraint does not apply", not as an error.
    pub fn confine(&self, from: (f64, f64), to: (f64, f64)) -> Option<(f64, f64)> {
        let mut x_out = 0.0;
        let mut y_out = 0.0;
        // SAFETY: `self` is live and only read; the out-parameters point at
        // live locals. wlroots leaves them untouched on the false path, which
        // is why they are initialised here rather than assumed written.
        let confined = unsafe {
            sys::wlr_region_confine(
                self.as_ptr(),
                from.0,
                from.1,
                to.0,
                to.1,
                &raw mut x_out,
                &raw mut y_out,
            )
        };
        confined.then_some((x_out, y_out))
    }
}

impl Default for Region {
    fn default() -> Region {
        Region::new()
    }
}

impl Clone for Region {
    fn clone(&self) -> Region {
        let mut out = Region::new();
        // SAFETY: `out` is freshly initialised and distinct from `self`, which
        // is live and only read.
        unsafe { pixman_region32_copy(out.as_mut_ptr(), self.as_ptr()) };
        out
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: every constructor initialises the region, and nothing else
        // finishes it, so this runs exactly once on an initialised region.
        unsafe { pixman_region32_fini(&raw mut *self.inner) };
    }
}

impl PartialEq for Region {
    fn eq(&self, other: &Region) -> bool {
        // SAFETY: both regions are live and initialised, and only read.
        unsafe { pixman_region32_equal(self.as_ptr(), other.as_ptr()) != 0 }
    }
}

impl Eq for Region {}

impl std::fmt::Debug for Region {
    /// Prints the extents and the rectangle count, not the whole
    /// decomposition: a full-screen damage region can hold thousands of
    /// rectangles and a `{:?}` in a log should not be that.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Region")
            .field("extents", &self.extents())
            .field("rectangles", &self.rectangles().count())
            .finish()
    }
}

impl FromIterator<Box2D> for Region {
    fn from_iter<I: IntoIterator<Item = Box2D>>(iter: I) -> Region {
        Region::from_boxes(&iter.into_iter().collect::<Vec<_>>())
    }
}

/// A borrowed view of a region wlroots owns.
///
/// Read-only, and scoped to the borrow that produced it exactly as this crate's
/// handles are: a region reached through a wlroots object (`wlr_damage_ring`'s
/// accumulated damage, a surface's buffer damage) is freed when that object is,
/// so it must not be stored. Use [`to_owned`](Self::to_owned) for a copy that
/// can be.
pub struct RegionRef<'a> {
    raw: NonNull<sys::pixman_region32>,
    _scope: PhantomData<&'a ()>,
}

impl<'a> RegionRef<'a> {
    /// Wrap a region wlroots owns.
    ///
    /// # Safety
    ///
    /// `raw` must point at a live, initialised `pixman_region32`, and `'a` must
    /// not outlive it. This never takes ownership: the region is not finished
    /// when the view drops.
    ///
    /// Minted by [`DamageRing::current`](crate::DamageRing::current) and its
    /// borrowed twin, and by
    /// [`SceneBuffer::opaque_region`](crate::SceneBuffer::opaque_region) — the
    /// regions wlroots embeds in its own structs by value.
    pub(crate) unsafe fn from_raw(raw: *mut sys::pixman_region32) -> RegionRef<'a> {
        RegionRef {
            raw: NonNull::new(raw).expect("wlroots handed us a null pixman_region32"),
            _scope: PhantomData,
        }
    }

    fn as_ptr(&self) -> *const sys::pixman_region32 {
        self.raw.as_ptr().cast_const()
    }

    /// Copy the region out, so it can be stored past this borrow.
    pub fn to_owned(&self) -> Region {
        let mut out = Region::new();
        // SAFETY: `out` is freshly initialised and distinct from the source,
        // which the handle's lifetime guarantees is live.
        unsafe { pixman_region32_copy(out.as_mut_ptr(), self.as_ptr()) };
        out
    }

    /// True when the region covers nothing.
    pub fn is_empty(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the region is live.
        unsafe { read::is_empty(self.as_ptr()) }
    }

    /// The smallest box containing every rectangle.
    pub fn extents(&self) -> Box2D {
        // SAFETY: as above.
        unsafe { read::extents(self.as_ptr()) }
    }

    /// True when `(x, y)` is covered.
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        // SAFETY: as above.
        unsafe { read::contains_point(self.as_ptr(), x, y) }
    }

    /// The rectangles making up the region.
    pub fn rectangles(&self) -> impl Iterator<Item = Box2D> + use<'_> {
        // SAFETY: the handle's lifetime guarantees the region is live, and the
        // borrow of `self` outlives the returned iterator.
        //
        // The third clause this used to carry — "nothing reachable from this
        // view can modify the region" — was not true, and was the whole
        // argument. `read::rectangles` mints a `&'static [pixman_box32]` that
        // this re-binds to `&self`, so the iterator is only as sound as the
        // guarantee that nothing mutates the region while it lives. Owners
        // that expose *shared*-reference mutators broke exactly that:
        // `DamageRing::current(&self)` and `add_box(&self)` were both shared,
        // so both could be live at once, and `pixman_region32_union`
        // reallocates the box array the iterator is walking. A use-after-free
        // reachable with no `unsafe` written by the consumer.
        //
        // The fix belongs at the owners, and is that every mutator on a type
        // that also hands out a borrowed `RegionRef` takes `&mut self` — so
        // the aliasing is a borrow error. That is a standing obligation on
        // anything that grows a `RegionRef` accessor, not a property this
        // method can check, which is why it is written here where the
        // `'static` is minted. `tests/ui/damage_ring_mutated_during_iteration.rs`
        // pins it.
        unsafe { read::rectangles(self.as_ptr()) }
            .iter()
            .copied()
            .map(box_from_pixman)
    }
}

impl std::fmt::Debug for RegionRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegionRef")
            .field("extents", &self.extents())
            .field("rectangles", &self.rectangles().count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corner-pair/origin-extent conversion, pinned by an explicit case in
    /// both directions rather than left to a round trip that would pass even if
    /// both halves were wrong the same way.
    #[test]
    fn pixman_box_conversion_is_corners_to_origin_and_extent() {
        let corners = sys::pixman_box32 {
            x1: 10,
            y1: 20,
            x2: 40,
            y2: 25,
        };
        assert_eq!(box_from_pixman(corners), Box2D::new(10, 20, 30, 5));

        let converted = box_to_pixman(Box2D::new(10, 20, 30, 5));
        assert_eq!((converted.x1, converted.y1), (10, 20));
        assert_eq!((converted.x2, converted.y2), (40, 25));
    }

    /// A negative extent must not become an enormous unsigned one on the way
    /// into pixman.
    #[test]
    fn a_negative_extent_becomes_an_empty_region_not_a_huge_one() {
        let region = Region::from_box(Box2D::new(0, 0, -5, 10));
        assert!(region.is_empty());

        let converted = box_to_pixman(Box2D::new(0, 0, -5, 10));
        assert_eq!(converted.x2, 0, "the corner pair is not inverted");
    }

    /// A `RegionRef` is a borrow of a region this crate owns elsewhere;
    /// finishing it is the owner's job, never the view's.
    #[test]
    fn a_region_ref_reads_without_taking_ownership() {
        let owned = Region::from_box(Box2D::new(1, 2, 3, 4));

        // SAFETY: `owned` outlives `view`, and the pointer is to its live,
        // initialised region. The cast to `*mut` is sound because nothing
        // reachable from `RegionRef` mutates.
        let view = unsafe { RegionRef::from_raw(owned.as_ptr().cast_mut()) };

        assert_eq!(view.extents(), Box2D::new(1, 2, 3, 4));
        assert!(!view.is_empty());
        let copied = view.to_owned();
        assert_eq!(copied, owned);

        // The owner is still intact once the view is gone: a `RegionRef` has no
        // `Drop` at all, which is the whole of "borrowed, not owned" — it
        // cannot finish the region even by accident.
        assert_eq!(owned.extents(), Box2D::new(1, 2, 3, 4));
        assert_eq!(copied.extents(), Box2D::new(1, 2, 3, 4));
    }
}
