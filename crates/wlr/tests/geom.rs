//! `Box2D`, `FBox` and `Transform` against real wlroots.
//!
//! These predicates are thin — four field comparisons, at most — which is
//! exactly why they are tested here rather than trusted. Every one of them is
//! an FFI call, so what is under test is not the arithmetic but that the Rust
//! types this crate hands to wlroots have the layout wlroots reads, and that
//! the crate reports wlroots' edge-case answers rather than the intuitive ones.
//!
//! The layout claim also has a compile-time half, in `src/geom.rs`'s `const _`
//! assertions against `sys::wlr_box` / `sys::wlr_fbox`. This file is the
//! behavioural half: every field here is given a distinct value, so a
//! transposition that a size check would miss produces a wrong answer.

use wlr::{Box2D, FBox, Transform};

const ALL: [Transform; 8] = [
    Transform::Normal,
    Transform::R90,
    Transform::R180,
    Transform::R270,
    Transform::Flipped,
    Transform::Flipped90,
    Transform::Flipped180,
    Transform::Flipped270,
];

#[test]
fn every_transform_composes_with_its_inverse_to_normal() {
    for t in ALL {
        assert_eq!(
            t.compose(t.invert()),
            Transform::Normal,
            "{t:?} then its inverse"
        );
        assert_eq!(
            t.invert().compose(t),
            Transform::Normal,
            "the inverse of {t:?} then {t:?}"
        );
    }
}

#[test]
fn invert_is_an_involution_and_a_bijection() {
    let mut inverses: Vec<Transform> = ALL.iter().map(|t| t.invert()).collect();
    inverses.sort_by_key(|t| t.to_raw());
    inverses.dedup();
    assert_eq!(inverses.len(), 8, "every transform has a distinct inverse");

    for t in ALL {
        assert_eq!(t.invert().invert(), t, "{t:?}");
    }
}

/// Composition order matters once a flip is in play. Getting `compose`'s
/// argument order backwards is the mistake this catches; a rotation-only pair
/// would not, since those commute.
#[test]
fn composing_a_flip_with_a_rotation_does_not_commute() {
    let a = Transform::Flipped.compose(Transform::R90);
    let b = Transform::R90.compose(Transform::Flipped);
    assert_ne!(a, b, "flip-then-rotate must differ from rotate-then-flip");
    assert_eq!(
        a,
        Transform::Flipped90,
        "wlroots applies the receiver first"
    );
}

#[test]
fn transform_round_trips_through_its_raw_value() {
    for t in ALL {
        assert_eq!(Transform::from_raw(t.to_raw()), Some(t));
    }
    assert_eq!(Transform::from_raw(8), None, "the range is closed at 7");
}

/// `wlr_output_transform_coords` swaps the pair for the four transforms that
/// exchange the axes and leaves it alone for the other four. It is *not* a
/// point rotation, despite its name — `R180` is a no-op — which is why
/// `Transform::apply_coords`' doc says so at length. This pins all eight, so
/// the crate cannot quietly start claiming the intuitive behaviour.
#[test]
fn apply_coords_swaps_the_axes_and_nothing_else() {
    for t in [
        Transform::Normal,
        Transform::R180,
        Transform::Flipped,
        Transform::Flipped180,
    ] {
        assert_eq!(t.apply_coords(3, 7), (3, 7), "{t:?} leaves the pair alone");
    }
    for t in [
        Transform::R90,
        Transform::R270,
        Transform::Flipped90,
        Transform::Flipped270,
    ] {
        assert_eq!(t.apply_coords(3, 7), (7, 3), "{t:?} swaps the pair");
    }
}

/// The layout claim, behaviourally: four distinct field values, and an
/// operation whose answer depends on all four.
#[test]
fn box_fields_reach_wlroots_in_the_right_order() {
    let b = Box2D::new(10, 20, 30, 40);
    assert!(b.contains_point(10.0, 20.0), "the origin is inside");
    assert!(
        b.contains_point(39.0, 59.0),
        "one pixel short of the far corner"
    );
    assert!(!b.contains_point(9.0, 20.0), "one pixel left of the origin");
    assert!(!b.contains_point(10.0, 19.0), "one pixel above the origin");
    assert_eq!(<(i32, i32, i32, i32)>::from(b), (10, 20, 30, 40));
    assert_eq!(Box2D::from((10, 20, 30, 40)), b);
}

/// wlroots' containment is half-open: the far edge is outside. This is the
/// example its own header gives, and getting it wrong shows up as a
/// one-pixel-wide input dead zone nobody can reproduce.
#[test]
fn contains_point_is_half_open() {
    let b = Box2D::new(0, 0, 100, 50);
    assert!(!b.contains_point(100.0, 50.0), "the far corner is outside");
    assert!(!b.contains_point(100.0, 25.0), "so is the right edge");
    assert!(!b.contains_point(50.0, 50.0), "and the bottom edge");
    assert!(b.contains_point(99.0, 49.0), "one pixel inside is inside");

    let offset = Box2D::new(10, 0, 50, 50);
    assert!(offset.contains_point(10.0, 10.0), "the near edge is inside");
}

#[test]
fn an_empty_box_contains_nothing_and_is_contained_by_nothing() {
    let big = Box2D::new(0, 0, 100, 100);
    for empty in [
        Box2D::default(),
        Box2D::new(10, 10, 0, 10),
        Box2D::new(10, 10, 10, 0),
        Box2D::new(10, 10, -1, -1),
    ] {
        assert!(empty.empty(), "{empty:?}");
        assert!(
            !big.contains_box(&empty),
            "wlroots says false when either box is empty, not true: {empty:?}"
        );
        assert!(!empty.contains_box(&big), "{empty:?}");
        assert!(!empty.contains_point(10.0, 10.0), "{empty:?}");
    }

    assert!(!big.empty());
    assert!(big.contains_box(&Box2D::new(10, 10, 10, 10)));
    assert!(!big.contains_box(&Box2D::new(90, 90, 20, 20)), "overhangs");
}

/// wlroots writes NaN into both out-parameters for an empty box. Handing that
/// to a consumer would poison every later coordinate computation silently.
#[test]
fn closest_point_reports_none_rather_than_a_nan() {
    assert_eq!(Box2D::default().closest_point(5.0, 5.0), None);
    assert_eq!(Box2D::new(1, 1, 0, 0).closest_point(5.0, 5.0), None);

    let b = Box2D::new(0, 0, 10, 10);
    assert_eq!(b.closest_point(5.0, 5.0), Some((5.0, 5.0)), "inside");
    let (x, y) = b
        .closest_point(-100.0, 5.0)
        .expect("a non-empty box has one");
    assert_eq!((x, y), (0.0, 5.0), "clamped to the near edge");
    assert!(!x.is_nan() && !y.is_nan());
}

#[test]
fn intersection_of_disjoint_boxes_is_none() {
    let a = Box2D::new(0, 0, 10, 10);
    assert_eq!(a.intersection(&Box2D::new(20, 20, 10, 10)), None);
    assert_eq!(
        a.intersection(&Box2D::new(10, 0, 10, 10)),
        None,
        "touching along an edge does not overlap, the box being half-open"
    );
    assert_eq!(a.intersection(&Box2D::default()), None, "empty");
    assert_eq!(
        a.intersection(&Box2D::new(5, 5, 10, 10)),
        Some(Box2D::new(5, 5, 5, 5))
    );
    assert_eq!(a.intersection(&a), Some(a), "with itself");
}

/// `wlr_box_equal` is field-wise **except** that it calls any two empty boxes
/// equal, however differently they are spelled. This crate keeps both answers
/// — `equals` for wlroots' and `==` for the fields — and this pins exactly
/// where the two part company, so neither can drift into the other.
#[test]
fn equals_is_field_wise_except_that_all_empty_boxes_are_equal() {
    let boxes = [
        Box2D::default(),
        Box2D::new(0, 0, 0, 0),
        Box2D::new(0, 0, 1, 1),
        Box2D::new(1, 0, 0, 0),
        Box2D::new(0, 1, 0, 0),
        Box2D::new(0, 0, 1, 0),
        Box2D::new(0, 0, 0, 1),
        Box2D::new(-1, -1, 2, 2),
        Box2D::new(10, 20, 30, 40),
        Box2D::new(20, 10, 30, 40),
        Box2D::new(10, 20, 40, 30),
        Box2D::new(i32::MIN, 0, 1, 1),
        Box2D::new(0, i32::MIN, 1, 1),
        Box2D::new(0, 0, i32::MAX, i32::MAX),
        Box2D::new(0, 0, -1, -1),
        Box2D::new(0, 0, -2, -2),
        Box2D::new(5, 5, 5, 5),
        Box2D::new(5, 5, 5, 6),
        Box2D::new(100, 50, 100, 50),
        Box2D::new(100, 50, 50, 100),
    ];
    assert_eq!(boxes.len(), 20);

    let mut differed = 0;
    for a in boxes {
        for b in boxes {
            if a.empty() && b.empty() {
                assert!(
                    a.equals(&b),
                    "wlroots calls any two empty boxes equal: {a:?} and {b:?}"
                );
                if a != b {
                    differed += 1;
                }
            } else {
                assert_eq!(
                    a.equals(&b),
                    a == b,
                    "away from the empty case wlr_box_equal is field-wise: {a:?} and {b:?}"
                );
            }
        }
    }
    assert!(
        differed > 0,
        "the fixture must actually contain differently-spelled empty boxes, \
         or this proves nothing about the exception"
    );
}

#[test]
fn transforming_a_box_within_a_frame_is_reversible() {
    let frame = (200, 100);
    let b = Box2D::new(10, 20, 30, 40);

    // A 180° turn in a frame is its own inverse, and lands the box on the
    // mirrored side of both axes.
    let turned = b.transformed(Transform::R180, frame.0, frame.1);
    assert_eq!(turned, Box2D::new(160, 40, 30, 40));
    assert_eq!(turned.transformed(Transform::R180, frame.0, frame.1), b);

    // A quarter turn swaps the extents.
    let quarter = b.transformed(Transform::R90, frame.0, frame.1);
    assert_eq!((quarter.width, quarter.height), (40, 30));

    assert_eq!(b.transformed(Transform::Normal, frame.0, frame.1), b);
}

#[test]
fn fbox_mirrors_the_integer_box() {
    assert!(FBox::default().empty());
    assert!(FBox::new(0.0, 0.0, 1.5, 0.0).empty());
    assert!(!FBox::new(0.0, 0.0, 0.5, 0.5).empty());

    let b = FBox::new(1.0, 2.0, 3.0, 4.0);
    assert!(b.equals(&FBox::new(1.0, 2.0, 3.0, 4.0)));
    assert!(!b.equals(&FBox::new(2.0, 1.0, 3.0, 4.0)));
    assert_eq!(b.equals(&b), b == b);
    assert!(
        FBox::default().equals(&FBox::new(5.0, 5.0, 0.0, 3.0)),
        "the empty-box exception applies to wlr_fbox_equal too"
    );

    let turned = b.transformed(Transform::R180, 10.0, 10.0);
    assert_eq!(turned, FBox::new(6.0, 4.0, 3.0, 4.0));
    assert_eq!(turned.transformed(Transform::R180, 10.0, 10.0), b);
}
