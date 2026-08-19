//! Damaging a ring while iterating its current region must not compile.
//!
//! `DamageRing::current` hands out a `RegionRef` borrowed from the ring, and
//! `rectangles()` iterates pixman's own box array in place. `add_box` runs
//! `pixman_region32_union`, which reallocates that array — so if both could be
//! live at once, the iterator would read freed memory with no `unsafe` written
//! here. The mutators take `&mut self` precisely so this is a borrow error
//! rather than a use-after-free.

fn main() {
    let mut ring = wlr::DamageRing::new();
    ring.add_box(wlr::Box2D::new(0, 0, 10, 10));

    let current = ring.current();
    let mut rects = current.rectangles();

    // The reallocation, while `rects` still points into the old array.
    ring.add_box(wlr::Box2D::new(100, 100, 10, 10));

    let _ = rects.next();
}
