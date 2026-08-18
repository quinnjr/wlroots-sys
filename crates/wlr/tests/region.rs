//! `Region` against real pixman and real wlroots.
//!
//! Two things are under test that a unit test could not reach. The first is
//! that the `pixman_region32_*` declarations in `src/region.rs` — hand-written,
//! since `wlr-sys` binds pixman's types but not its functions — match the ABI
//! of the `libpixman-1` this crate links. A wrong signature there compiles and
//! links and then corrupts memory, so every declaration is exercised at least
//! once here. The second is wlroots' own `wlr_region_*` helpers, whose rounding
//! and unit conventions are documented nowhere in the headers.

use wlr::{Box2D, Region, Transform};

#[test]
fn a_fresh_region_is_empty() {
    let r = Region::new();
    assert!(r.is_empty());
    assert_eq!(r.rectangles().count(), 0);
    assert!(r.extents().empty(), "an empty region has empty extents");
    assert!(!r.contains_point(0, 0));
    assert_eq!(r, Region::default());
}

#[test]
fn from_box_round_trips_through_rectangles() {
    let b = Box2D::new(10, 20, 30, 40);
    let r = Region::from_box(b);

    assert!(!r.is_empty());
    assert_eq!(r.rectangles().collect::<Vec<_>>(), vec![b]);
    assert_eq!(r.extents(), b);
    assert!(r.contains_point(10, 20), "the origin is inside");
    assert!(!r.contains_point(40, 60), "the far corner is not");
}

/// Two disjoint boxes stay two rectangles; the conversion between pixman's
/// corner pairs and this crate's origin-and-extent boxes is what would break
/// here, and it would break by producing plausible-looking wrong numbers.
#[test]
fn from_boxes_keeps_disjoint_rectangles_apart() {
    let boxes = [Box2D::new(0, 0, 5, 5), Box2D::new(10, 0, 5, 5)];
    let r = Region::from_boxes(&boxes);

    assert_eq!(r.rectangles().collect::<Vec<_>>(), boxes);
    assert_eq!(r.extents(), Box2D::new(0, 0, 15, 5));
    assert!(r.contains_point(0, 0));
    assert!(!r.contains_point(7, 2), "the gap is not covered");
    assert!(r.contains_point(14, 4));

    assert_eq!(r, boxes.into_iter().collect::<Region>());
}

/// pixman merges and re-decomposes: what comes back is the canonical band
/// decomposition, not the input. A test asserting an exact round trip would
/// pass today and fail the first time a caller passed overlapping boxes.
#[test]
fn from_boxes_merges_overlapping_input() {
    let r = Region::from_boxes(&[Box2D::new(0, 0, 10, 10), Box2D::new(5, 0, 10, 10)]);
    assert_eq!(
        r.rectangles().collect::<Vec<_>>(),
        vec![Box2D::new(0, 0, 15, 10)]
    );
}

#[test]
fn set_operations_match_hand_computed_answers() {
    let a = Region::from_box(Box2D::new(0, 0, 10, 10));
    let b = Region::from_box(Box2D::new(5, 5, 10, 10));

    // The union of two overlapping squares is three bands.
    assert_eq!(
        a.union(&b).rectangles().collect::<Vec<_>>(),
        vec![
            Box2D::new(0, 0, 10, 5),
            Box2D::new(0, 5, 15, 5),
            Box2D::new(5, 10, 10, 5),
        ]
    );
    assert_eq!(
        a.intersect(&b).rectangles().collect::<Vec<_>>(),
        vec![Box2D::new(5, 5, 5, 5)]
    );
    assert_eq!(
        a.subtract(&b).rectangles().collect::<Vec<_>>(),
        vec![Box2D::new(0, 0, 10, 5), Box2D::new(0, 5, 5, 5)]
    );

    // Neither source is disturbed: every operation writes into a fresh
    // destination rather than aliasing one of its inputs.
    assert_eq!(a, Region::from_box(Box2D::new(0, 0, 10, 10)));
    assert_eq!(b, Region::from_box(Box2D::new(5, 5, 10, 10)));

    let disjoint = Region::from_box(Box2D::new(100, 100, 1, 1));
    assert!(a.intersect(&disjoint).is_empty());
    assert_eq!(a.subtract(&disjoint), a);
    assert_eq!(a.subtract(&a), Region::new());
}

#[test]
fn translate_moves_every_rectangle() {
    let r = Region::from_boxes(&[Box2D::new(0, 0, 5, 5), Box2D::new(10, 0, 5, 5)]);
    let moved = r.translated(3, -2);

    assert_eq!(
        moved.rectangles().collect::<Vec<_>>(),
        vec![Box2D::new(3, -2, 5, 5), Box2D::new(13, -2, 5, 5)]
    );
    assert_eq!(moved.translated(-3, 2), r, "and it is reversible");
    assert_eq!(
        r.rectangles().next(),
        Some(Box2D::new(0, 0, 5, 5)),
        "the source is untouched"
    );
}

#[test]
fn clone_and_equality_are_by_content_not_by_pointer() {
    let r = Region::from_boxes(&[Box2D::new(0, 0, 5, 5), Box2D::new(10, 0, 5, 5)]);
    let copy = r.clone();

    assert_eq!(copy, r);
    assert_eq!(
        copy.rectangles().collect::<Vec<_>>(),
        r.rectangles().collect::<Vec<_>>()
    );

    // Independent storage: mutating one (by deriving a new region from it)
    // leaves the other alone, and dropping one does not free the other's data.
    drop(r);
    assert_eq!(copy.extents(), Box2D::new(0, 0, 15, 5));

    assert_ne!(copy, Region::from_box(Box2D::new(0, 0, 15, 5)));
}

/// `wlr_region_scale` rounds **outward**, so the result covers the scaled input
/// rather than approximating it — the property damage tracking depends on, and
/// the one a naive `(x * scale) as i32` would break.
#[test]
fn scaling_rounds_outward() {
    let r = Region::from_box(Box2D::new(1, 1, 3, 3));

    let doubled = r.scaled(2.0);
    assert_eq!(doubled.extents(), Box2D::new(2, 2, 6, 6));

    let one_and_a_half = r.scaled(1.5);
    let extents = one_and_a_half.extents();
    assert!(
        extents.x <= 1 && extents.y <= 1,
        "the near edges round down: {extents:?}"
    );
    assert!(
        extents.x + extents.width >= 6 && extents.y + extents.height >= 6,
        "the far edges round up, covering 1.5 * 4 = 6: {extents:?}"
    );

    let stretched = r.scaled_xy(2.0, 1.0);
    assert_eq!(stretched.extents(), Box2D::new(2, 1, 6, 3));
}

#[test]
fn transforming_a_region_matches_transforming_its_box() {
    let b = Box2D::new(10, 20, 30, 40);
    let r = Region::from_box(b);

    for t in [
        Transform::Normal,
        Transform::R90,
        Transform::R180,
        Transform::R270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ] {
        assert_eq!(
            r.transformed(t, 200, 100).extents(),
            b.transformed(t, 200, 100),
            "{t:?}"
        );
    }
}

#[test]
fn expanding_by_zero_is_a_no_op_and_expanding_grows_every_side() {
    let r = Region::from_box(Box2D::new(10, 10, 5, 5));

    assert_eq!(r.expanded(0), r, "zero distance changes nothing");
    assert_eq!(r.expanded(2).extents(), Box2D::new(8, 8, 9, 9));
    assert!(
        r.expanded(2).contains_point(9, 9),
        "a point outside the original is inside the expansion"
    );
}

/// wlroots' header does not say whether `wlr_region_rotated_bounds` takes
/// radians or degrees, and both readings are plausible. Radians: rotating a
/// region in the first quadrant by π about the origin lands it in the third.
/// Degrees would read the same value as a 3.14° nudge, leaving it in the first.
#[test]
fn rotated_bounds_takes_radians() {
    let r = Region::from_box(Box2D::new(0, 0, 10, 4));

    let half_turn = r.rotated_bounds(std::f32::consts::PI, 0, 0);
    assert!(
        half_turn.contains_point(-9, -3),
        "π radians is a half turn, putting the region in the opposite quadrant: {:?}",
        half_turn.extents()
    );

    let nudged = r.rotated_bounds(180.0, 0, 0);
    assert!(
        !nudged.contains_point(-9, -3),
        "and 180.0 is not a half turn, which is what makes the first assertion \
         say something: {:?}",
        nudged.extents()
    );

    assert_eq!(
        r.rotated_bounds(0.0, 0, 0).extents(),
        Box2D::new(0, 0, 10, 4),
        "no rotation leaves the bounds alone"
    );
}

/// `confine` answers "given that the pointer was inside, where does this move
/// put it?" — and reports `None`, not an error, when the premise is false.
#[test]
fn confine_clamps_from_inside_and_declines_from_outside() {
    let r = Region::from_box(Box2D::new(0, 0, 10, 10));

    assert_eq!(
        r.confine((1.0, 1.0), (50.0, 3.0)),
        Some((9.0, 3.0)),
        "the move is clamped to the region's far edge"
    );
    assert_eq!(
        r.confine((1.0, 1.0), (5.0, 5.0)),
        Some((5.0, 5.0)),
        "a move that stays inside is unchanged"
    );
    assert_eq!(
        r.confine((-1.0, -1.0), (5.0, 5.0)),
        None,
        "the old position was not inside, so there is nothing to confine"
    );
}

#[test]
fn debug_prints_a_summary_rather_than_every_rectangle() {
    let r = Region::from_boxes(&[Box2D::new(0, 0, 5, 5), Box2D::new(10, 0, 5, 5)]);
    let printed = format!("{r:?}");
    assert!(printed.contains("Region"), "{printed}");
    assert!(printed.contains("rectangles: 2"), "{printed}");
}
