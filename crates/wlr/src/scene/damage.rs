//! Damage rings: what changed since the buffer you are about to draw into was
//! last presented.
//!
//! # Two shapes, because there are two owners
//!
//! `wlr_damage_ring` is a **caller-allocated** value — `init`/`finish`, not
//! `create`/`destroy` — and wlroots embeds one inside every
//! `wlr_scene_output`. So a wrapper has to answer for both cases and cannot
//! answer for both at once:
//!
//! - [`DamageRing`] is a ring this crate allocated. It has a [`Drop`] calling
//!   `wlr_damage_ring_finish`, one of only two places in this crate where a
//!   `Drop` on a wlroots type is right (the other is
//!   [`SceneTimer`](crate::SceneTimer)).
//! - [`DamageRingRef`] is a borrowed view of a ring wlroots owns — most of all
//!   [`SceneOutput::damage_ring`](crate::SceneOutput::damage_ring). It has no
//!   `Drop` and no way to reach `init` or `finish`, because calling either on a
//!   ring the scene owns would corrupt it.
//!
//! # Why the ring is boxed
//!
//! `wlr_damage_ring`'s private half is a `wl_list` head, and a `wl_list` head's
//! neighbours point back at it: every `wlr_damage_ring_buffer` the ring has
//! seen holds a `link` whose `prev`/`next` name that head's address. Moving the
//! struct after `init` therefore leaves those pointers naming the old address —
//! a use-after-free the moment the ring is walked. A [`Box`] is what makes the
//! address stable: moving a `DamageRing` value moves the box, not the ring.
//! [`a_moved_ring_is_still_coherent`](tests) pins that.
//!
//! The box holds an [`UnsafeCell`] because every mutator here takes `&self`.
//! wlroots mutates the ring through the pointer it is handed, and deriving a
//! `*mut` from a shared reference to write through is the sort of provenance
//! bug that runs correctly for years — the `UnsafeCell` is the declaration that
//! makes it sound rather than merely untested.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::geom::Box2D;
use crate::region::{Region, RegionRef};
use crate::{Buffer, sys};

/// The operations both shapes share, so neither can drift from the other.
///
/// # Safety
///
/// Every function here requires `ring` to point at a live ring on which
/// `wlr_damage_ring_init` has run and `wlr_damage_ring_finish` has not.
mod ops {
    use super::{Box2D, Buffer, Region, RegionRef, sys};

    pub(super) unsafe fn add(ring: *mut sys::wlr_damage_ring, region: &Region) {
        // SAFETY: the caller guarantees `ring` is live and initialised;
        // `region` is live for the call and wlroots only reads it (it unions
        // the rectangles into the ring's own `current`).
        unsafe { sys::wlr_damage_ring_add(ring, region.as_ptr()) };
    }

    pub(super) unsafe fn add_box(ring: *mut sys::wlr_damage_ring, area: Box2D) {
        // SAFETY: as above. `area` is a live local whose layout is pinned to
        // `wlr_box` (see `geom.rs`), and wlroots copies out of it.
        unsafe { sys::wlr_damage_ring_add_box(ring, area.as_c()) };
    }

    pub(super) unsafe fn add_whole(ring: *mut sys::wlr_damage_ring) {
        // SAFETY: as above; this takes no pointer but the ring's own.
        unsafe { sys::wlr_damage_ring_add_whole(ring) };
    }

    /// The accumulated damage, as a borrow of the ring's own `current`.
    ///
    /// # Safety
    ///
    /// As above, plus: `'a` must not outlive the ring.
    pub(super) unsafe fn current<'a>(ring: *mut sys::wlr_damage_ring) -> RegionRef<'a> {
        // SAFETY: `current` is a `pixman_region32` embedded in the ring by
        // value and initialised by `wlr_damage_ring_init` for the ring's whole
        // life, and the caller bounds `'a` by the ring's own lifetime.
        unsafe { RegionRef::from_raw(&raw mut (*ring).current) }
    }

    pub(super) unsafe fn rotate_buffer(
        ring: *mut sys::wlr_damage_ring,
        buffer: &Buffer<'_>,
    ) -> Region {
        let mut out = Region::new();
        // SAFETY: the caller guarantees `ring` is live and initialised; the
        // handle's lifetime guarantees the buffer is live; and `out` is a
        // freshly *initialised* region, which is what
        // `wlr_damage_ring_rotate_buffer`'s out-parameter requires — it unions
        // into the destination rather than initialising it.
        unsafe { sys::wlr_damage_ring_rotate_buffer(ring, buffer.as_ptr(), out.as_mut_ptr()) };
        out
    }
}

/// A damage ring this crate owns.
///
/// Accumulates damage in **buffer-local** coordinates until
/// [`rotate_buffer`](Self::rotate_buffer) hands it out and rotates the ring on
/// to the next buffer. A compositor rendering into its own swapchain keeps one
/// of these per output; a compositor letting the scene render for it does not
/// need one at all, because [`SceneOutput`](crate::SceneOutput) has its own.
///
/// `!Send` and `!Sync`, like everything in this crate that owns a C allocation.
pub struct DamageRing {
    inner: Box<UnsafeCell<sys::wlr_damage_ring>>,
}

impl DamageRing {
    /// A fresh ring with no damage recorded.
    pub fn new() -> DamageRing {
        // Zeroed rather than `MaybeUninit`: `wlr_damage_ring` is a
        // `pixman_region32` and a `wl_list`, both of which are valid Rust
        // values when zeroed, and `wlr_damage_ring_init` overwrites the whole
        // struct anyway.
        //
        // SAFETY: all-zero is a valid bit pattern for the struct above.
        let inner = Box::new(UnsafeCell::new(unsafe {
            std::mem::zeroed::<sys::wlr_damage_ring>()
        }));
        // SAFETY: `inner` is a fresh, uniquely-owned allocation at an address
        // that will not move again (see the module doc), and
        // `wlr_damage_ring_init` initialises it in place.
        unsafe { sys::wlr_damage_ring_init(inner.get()) };
        DamageRing { inner }
    }

    /// The raw ring. Valid for as long as this value is.
    ///
    /// Stable across every move of the `DamageRing` itself — the ring lives in
    /// a box, so a move moves the box.
    pub fn as_ptr(&self) -> *mut sys::wlr_damage_ring {
        self.inner.get()
    }

    /// Add `region`, in buffer-local coordinates, to the accumulated damage.
    pub fn add(&self, region: &Region) {
        // SAFETY: this value's own constructor initialised the ring and only
        // `Drop` finishes it, so it is live and initialised here.
        unsafe { ops::add(self.as_ptr(), region) };
    }

    /// Add `area`, in buffer-local coordinates, to the accumulated damage.
    pub fn add_box(&mut self, area: Box2D) {
        // SAFETY: as for `add`.
        unsafe { ops::add_box(self.as_ptr(), area) };
    }

    /// Damage everything the ring has ever seen a buffer as large as.
    ///
    /// Not "damage infinity", which is the reading the name invites and the one
    /// that is wrong. wlroots implements this by walking the buffers the ring
    /// has rotated through, taking the largest width and the largest height
    /// among them, and unioning that rectangle in (verified by disassembling
    /// `libwlroots-0.20.so`). So **on a ring that has never rotated a buffer
    /// this does nothing at all** — the maximum over an empty list is `0 × 0` —
    /// and [`add_box`](Self::add_box) with the size you are about to render is
    /// the call that works before the first frame.
    pub fn add_whole(&mut self) {
        // SAFETY: as for `add`.
        unsafe { ops::add_whole(self.as_ptr()) };
    }

    /// The damage accumulated since the last rotation.
    ///
    /// Borrowed from the ring, so it cannot outlive this value; use
    /// [`RegionRef::to_owned`] for a copy that can.
    pub fn current(&self) -> RegionRef<'_> {
        // SAFETY: as for `add`, and the returned view's lifetime is this
        // borrow of `self`.
        unsafe { ops::current(self.as_ptr()) }
    }

    /// Take the damage `buffer` needs redrawn, and rotate the ring.
    ///
    /// The result is the difference between what `buffer` holds and what is
    /// about to be painted — in buffer-local coordinates — which is exactly the
    /// region a renderer must cover. Rotating is not undoable: **if rendering
    /// or submission then fails, re-damage the ring**, or the next frame will
    /// believe those pixels are already correct.
    pub fn rotate_buffer(&mut self, buffer: &Buffer<'_>) -> Region {
        // SAFETY: as for `add`; the handle's lifetime guarantees the buffer is
        // live for the call.
        unsafe { ops::rotate_buffer(self.as_ptr(), buffer) }
    }
}

impl Default for DamageRing {
    fn default() -> DamageRing {
        DamageRing::new()
    }
}

impl std::fmt::Debug for DamageRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DamageRing")
            .field("current", &self.current())
            .finish()
    }
}

impl Drop for DamageRing {
    fn drop(&mut self) {
        // SAFETY: this value initialised the ring in `new` and nothing else can
        // have finished it — `finish` is not reachable from anywhere else in
        // this crate, and `DamageRingRef` (the only other shape) has no way to
        // reach it at all.
        unsafe { sys::wlr_damage_ring_finish(self.as_ptr()) };
    }
}

/// A borrowed view of a damage ring wlroots owns.
///
/// Scoped to the borrow that produced it, exactly as this crate's handles are:
/// the ring inside a [`SceneOutput`](crate::SceneOutput) is freed when that
/// scene output is, so a view of it must not be stored. Everything
/// [`DamageRing`] can do except be created or finished — both of which belong
/// to the owner.
pub struct DamageRingRef<'a> {
    raw: NonNull<sys::wlr_damage_ring>,
    _scope: PhantomData<&'a ()>,
}

impl<'a> DamageRingRef<'a> {
    /// Wrap a ring wlroots owns.
    ///
    /// # Safety
    ///
    /// `raw` must point at a live ring that `wlr_damage_ring_init` has run on,
    /// and `'a` must not outlive it. This never takes ownership: the ring is
    /// not finished when the view drops.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_damage_ring) -> DamageRingRef<'a> {
        DamageRingRef {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_damage_ring"),
            _scope: PhantomData,
        }
    }

    /// The raw ring. Valid only for as long as the view is.
    pub fn as_ptr(&self) -> *mut sys::wlr_damage_ring {
        self.raw.as_ptr()
    }

    /// Add `region`, in buffer-local coordinates, to the accumulated damage.
    ///
    /// On a scene output's ring this is how a compositor tells the scene that
    /// something it does not know about changed; the scene damages the ring
    /// itself for everything it does know about.
    pub fn add(&self, region: &Region) {
        // SAFETY: the view's lifetime guarantees the ring is live, and a ring
        // reachable through a wlroots object has been initialised by its owner.
        unsafe { ops::add(self.as_ptr(), region) };
    }

    /// Add `area`, in buffer-local coordinates, to the accumulated damage.
    pub fn add_box(&mut self, area: Box2D) {
        // SAFETY: as for `add`.
        unsafe { ops::add_box(self.as_ptr(), area) };
    }

    /// Damage everything, as [`DamageRing::add_whole`] means it — the largest
    /// buffer this ring has rotated, and nothing at all before the first one.
    pub fn add_whole(&mut self) {
        // SAFETY: as for `add`.
        unsafe { ops::add_whole(self.as_ptr()) };
    }

    /// The damage accumulated since the last rotation.
    pub fn current(&self) -> RegionRef<'_> {
        // SAFETY: as for `add`; the returned view's lifetime is this borrow.
        unsafe { ops::current(self.as_ptr()) }
    }

    /// Take the damage `buffer` needs redrawn, and rotate the ring.
    ///
    /// The same one-way operation [`DamageRing::rotate_buffer`] documents,
    /// including the obligation to re-damage on a failed render. Rotating a
    /// scene output's ring by hand is for a compositor doing its own rendering
    /// off that output; [`Runtime::commit_scene_output`](crate::Runtime::commit_scene_output)
    /// rotates it itself.
    pub fn rotate_buffer(&mut self, buffer: &Buffer<'_>) -> Region {
        // SAFETY: as for `add`; the handle's lifetime guarantees the buffer is
        // live for the call.
        unsafe { ops::rotate_buffer(self.as_ptr(), buffer) }
    }
}

impl std::fmt::Debug for DamageRingRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DamageRingRef")
            .field("current", &self.current())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions::assert_not_impl_any;

    /// A ring owns a C allocation whose neighbours point back at it; sharing
    /// one across threads would be exactly the aliasing the crate's whole
    /// `IN_HANDLER` argument assumes cannot happen.
    #[test]
    fn damage_rings_are_thread_bound() {
        assert_not_impl_any!(DamageRing: Send, Sync);
        assert_not_impl_any!(DamageRingRef<'static>: Send, Sync);
    }

    /// The three ways damage goes in, read back through `current`.
    #[test]
    fn damage_accumulates_and_reads_back() {
        let mut ring = DamageRing::new();
        assert!(ring.current().is_empty(), "a fresh ring has no damage");

        ring.add_box(Box2D::new(0, 0, 10, 10));
        assert_eq!(ring.current().extents(), Box2D::new(0, 0, 10, 10));

        ring.add(&Region::from_box(Box2D::new(20, 0, 5, 5)));
        assert_eq!(
            ring.current().extents(),
            Box2D::new(0, 0, 25, 10),
            "the extents cover the union of both additions"
        );
    }

    /// The trap [`DamageRing::add_whole`]'s doc describes, pinned: "whole" is
    /// the largest buffer the ring has rotated, so on a ring that has rotated
    /// none it adds nothing. A test asserting the opposite is what caught this
    /// against the shipped library.
    #[test]
    fn add_whole_adds_nothing_to_a_ring_that_has_seen_no_buffer() {
        let mut ring = DamageRing::new();
        ring.add_whole();
        assert!(
            ring.current().is_empty(),
            "with no buffer rotated there is no size to damage"
        );

        ring.add_box(Box2D::new(0, 0, 10, 10));
        ring.add_whole();
        assert_eq!(
            ring.current().extents(),
            Box2D::new(0, 0, 10, 10),
            "and it still adds nothing on top of a hand-added box"
        );
    }

    /// The hazard the module doc describes, pinned: the ring's address must
    /// survive a move of the value that owns it, and the ring must still be
    /// usable afterwards.
    #[test]
    fn a_moved_ring_is_still_coherent() {
        let mut ring = DamageRing::new();
        ring.add_box(Box2D::new(1, 2, 3, 4));
        let address = ring.as_ptr();

        // The move that would break a ring held inline.
        let mut moved = ring;
        assert_eq!(
            moved.as_ptr(),
            address,
            "the ring lives in a box, so moving the value moves the box"
        );
        assert_eq!(moved.current().extents(), Box2D::new(1, 2, 3, 4));

        moved.add_box(Box2D::new(0, 0, 1, 1));
        assert_eq!(moved.current().extents(), Box2D::new(0, 0, 4, 6));
    }

    /// A view reads and writes the same ring, and finishing it is the owner's
    /// job alone — the view has no `Drop` at all.
    #[test]
    fn a_view_shares_the_owner_s_ring_without_owning_it() {
        let owned = DamageRing::new();
        {
            // SAFETY: `owned` outlives the view, and its ring was initialised
            // by `DamageRing::new`.
            let mut view = unsafe { DamageRingRef::from_raw(owned.as_ptr()) };
            view.add_box(Box2D::new(0, 0, 8, 8));
            assert_eq!(view.current().extents(), Box2D::new(0, 0, 8, 8));
        }
        assert_eq!(
            owned.current().extents(),
            Box2D::new(0, 0, 8, 8),
            "the owner sees what the view added, and still has its ring"
        );
    }
}
