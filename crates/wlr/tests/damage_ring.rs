//! Damage rings against a real wlroots: the owned shape, the borrowed shape,
//! and the rotation that hands accumulated damage to a renderer.
//!
//! The rotation half needs real `wlr_buffer`s, so it runs on the pixman
//! renderer and its shared-memory allocator — no GPU, the same choice
//! `tests/render.rs` makes and for the same reason.

use std::sync::Once;

use wlr::{
    Allocator, Backend, Box2D, DamageRing, Display, DrmFormat, FourCc, Modifier, Region, Renderer,
};

fn headless_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: single-threaded, before any other thread exists, and each
        // integration binary is its own process.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            std::env::set_var("WLR_RENDERER", "pixman");
        }
    });
}

fn argb() -> DrmFormat {
    DrmFormat::new(FourCc::ARGB8888, [Modifier::LINEAR])
}

/// The three ways damage goes in, and the one way it comes back out.
#[test]
fn a_ring_accumulates_every_shape_of_damage() {
    let ring = DamageRing::new();
    assert!(ring.current().is_empty());

    ring.add_box(Box2D::new(0, 0, 16, 16));
    assert_eq!(ring.current().extents(), Box2D::new(0, 0, 16, 16));

    ring.add(&Region::from_boxes(&[
        Box2D::new(32, 0, 4, 4),
        Box2D::new(0, 32, 4, 4),
    ]));
    assert_eq!(
        ring.current().extents(),
        Box2D::new(0, 0, 36, 36),
        "extents cover every rectangle added so far"
    );
    assert!(ring.current().contains_point(1, 1));
    assert!(
        !ring.current().contains_point(20, 20),
        "the extents are a bounding box; the region itself is not filled in"
    );

    // A copy taken out survives past the borrow the view was scoped to.
    let owned = ring.current().to_owned();
    assert_eq!(owned.extents(), Box2D::new(0, 0, 36, 36));
}

/// Rotation is the whole point of a *ring*: the damage handed out for a buffer
/// is the difference between that buffer and what is about to be painted, and
/// taking it empties the accumulator.
#[test]
fn rotating_a_buffer_takes_the_accumulated_damage_and_empties_it() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    let first = allocator.create_buffer(64, 64, &argb()).expect("buffer");
    let second = allocator.create_buffer(64, 64, &argb()).expect("buffer");

    let ring = DamageRing::new();
    ring.add_box(Box2D::new(8, 8, 16, 16));
    assert_eq!(ring.current().extents(), Box2D::new(8, 8, 16, 16));

    // A buffer the ring has never seen needs everything redrawn, whatever the
    // accumulated damage says.
    let full = ring.rotate_buffer(&first);
    assert!(
        !full.is_empty(),
        "a buffer the ring has not seen before is fully damaged"
    );
    assert!(
        ring.current().is_empty(),
        "rotating takes the accumulated damage rather than copying it"
    );

    // Now damage a known region and rotate the *other* buffer: it needs both
    // what changed since it was last painted and what has just been damaged.
    ring.add_box(Box2D::new(0, 0, 8, 8));
    let partial = ring.rotate_buffer(&second);
    assert!(partial.contains_point(1, 1));
    assert!(ring.current().is_empty());

    // Rotating the first buffer back in reports what changed while it was out
    // of rotation, not the whole buffer again.
    let again = ring.rotate_buffer(&first);
    assert!(
        again.contains_point(1, 1),
        "the region damaged while this buffer was out of rotation: {again:?}"
    );
}

/// `add_whole` is sized from the buffers the ring has rotated, so it does
/// nothing before the first one and covers the buffer afterwards — the trap
/// `DamageRing::add_whole`'s own doc spells out.
#[test]
fn add_whole_is_sized_by_the_buffers_the_ring_has_seen() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let buffer = allocator.create_buffer(32, 24, &argb()).expect("buffer");

    let ring = DamageRing::new();
    ring.add_whole();
    assert!(
        ring.current().is_empty(),
        "no buffer rotated yet, so there is no size to damage"
    );

    let _ = ring.rotate_buffer(&buffer);
    ring.add_whole();
    assert_eq!(
        ring.current().extents(),
        Box2D::new(0, 0, 32, 24),
        "once a buffer has been through, `whole` is that buffer's size"
    );
}

/// The ring lives in a box, so moving the value that owns it does not move the
/// `wl_list` head its rotated buffers point back at. A ring held inline would
/// be corrupt after this.
#[test]
fn a_ring_survives_being_moved_after_a_buffer_has_been_rotated() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let buffer = allocator.create_buffer(16, 16, &argb()).expect("buffer");

    let ring = DamageRing::new();
    let _ = ring.rotate_buffer(&buffer);
    let address = ring.as_ptr();

    // Move it into a struct, then out again: the list head must not follow.
    struct Holder {
        ring: DamageRing,
    }
    let holder = Holder { ring };
    let moved = holder.ring;
    assert_eq!(moved.as_ptr(), address);

    moved.add_box(Box2D::new(2, 2, 4, 4));
    assert_eq!(moved.current().extents(), Box2D::new(2, 2, 4, 4));
    // Walks the buffer list the move could have broken.
    let damage = moved.rotate_buffer(&buffer);
    assert!(damage.contains_point(3, 3));
}
