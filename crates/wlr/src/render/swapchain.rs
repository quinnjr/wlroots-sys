//! Swapchains: a small ring of buffers to render into.
//!
//! Four slots, allocated lazily. [`Swapchain::acquire`] hands out the
//! **consumer** reference — a [`LockedBuffer`], released with
//! `wlr_buffer_unlock` — and the slot stays busy until whoever holds the buffer
//! releases it. See `allocator.rs`'s own doc for why the producer and consumer
//! references are different types.

use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;

use crate::buffer::Buffer;
use crate::error::{Error, Result};
use crate::sys;

use super::allocator::{Allocator, AllocatorRef};
use super::format::{DrmFormat, DrmFormatRef};

/// `WLR_SWAPCHAIN_CAP`: how many buffers one swapchain can have in flight.
pub const SWAPCHAIN_CAP: usize = sys::WLR_SWAPCHAIN_CAP as usize;

/// A ring of buffers from one allocator. Dropping it calls
/// `wlr_swapchain_destroy`.
///
/// Borrows the allocator, which is what makes "the allocator died" unreachable
/// from safe code for a swapchain built on an [`Allocator`]. It is still
/// checkable — wlroots nulls `wlr_swapchain.allocator` from its own destroy
/// listener — and [`acquire`](Swapchain::acquire) reports it, because a
/// swapchain built on an [`AllocatorRef`] borrows a runtime, not an allocator.
pub struct Swapchain<'a> {
    raw: NonNull<sys::wlr_swapchain>,
    _allocator: PhantomData<&'a ()>,
}

impl<'a> Swapchain<'a> {
    /// Create a swapchain of `width` × `height` buffers in `format`.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] for a non-positive width or height —
    /// `wlr_swapchain_create` **asserts** both are positive, and the assertion
    /// is compiled into the shipped library. [`Error::Create`] if wlroots could
    /// not allocate the swapchain (note it does *not* allocate any buffer yet,
    /// so a format the allocator cannot serve fails later, at
    /// [`acquire`](Swapchain::acquire)).
    pub fn create(
        allocator: &'a Allocator<'_>,
        width: i32,
        height: i32,
        format: &DrmFormat,
    ) -> Result<Swapchain<'a>> {
        // SAFETY: the borrow guarantees the allocator is live for `'a`.
        unsafe { Swapchain::create_raw(allocator.as_ptr(), width, height, format) }
    }

    /// Create a swapchain on the allocator a
    /// [`Runtime`](crate::Runtime) owns.
    ///
    /// # Errors
    ///
    /// As [`Swapchain::create`].
    pub fn create_on_ref(
        allocator: AllocatorRef<'a>,
        width: i32,
        height: i32,
        format: &DrmFormat,
    ) -> Result<Swapchain<'a>> {
        // SAFETY: `AllocatorRef<'a>` guarantees the allocator is live for `'a`.
        unsafe { Swapchain::create_raw(allocator.as_ptr(), width, height, format) }
    }

    /// # Safety
    ///
    /// `allocator` must point at a live `wlr_allocator` that outlives `'a`.
    unsafe fn create_raw(
        allocator: *mut sys::wlr_allocator,
        width: i32,
        height: i32,
        format: &DrmFormat,
    ) -> Result<Swapchain<'a>> {
        if width <= 0 || height <= 0 {
            // Refused before the call rather than reported by it: wlroots
            // asserts `width > 0 && height > 0` (`render/swapchain.c`), and
            // that assertion is compiled into the distro's library.
            return Err(Error::Operation("Swapchain::create"));
        }
        // SAFETY: the caller guarantees the allocator is live; `with_sys` keeps
        // the temporary format — and the modifier array it borrows — alive for
        // the call, which is all wlroots needs, since it copies the format
        // (`wlr_drm_format_copy`) rather than retaining the pointer.
        let raw = format
            .with_sys(|fmt| unsafe { sys::wlr_swapchain_create(allocator, width, height, fmt) });
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_swapchain_create"))?;
        Ok(Swapchain {
            raw,
            _allocator: PhantomData,
        })
    }

    /// The raw swapchain, for interop with code that speaks `wlr-sys` directly.
    ///
    /// Borrowed, never owned.
    pub fn as_ptr(&self) -> *mut sys::wlr_swapchain {
        self.raw.as_ptr()
    }

    /// Buffer width in pixels.
    pub fn width(&self) -> i32 {
        // SAFETY: this value owns a live swapchain for as long as it exists.
        unsafe { (*self.raw.as_ptr()).width }
    }

    /// Buffer height in pixels.
    pub fn height(&self) -> i32 {
        // SAFETY: as above.
        unsafe { (*self.raw.as_ptr()).height }
    }

    /// The format the swapchain allocates in.
    ///
    /// Borrowed from the swapchain, which owns it and frees it in
    /// `wlr_swapchain_destroy` — never `wlr_drm_format_finish` this.
    pub fn format(&self) -> DrmFormatRef<'_> {
        // SAFETY: as above; the format is a field of the live swapchain and
        // cannot outlive this borrow.
        unsafe { DrmFormatRef::from_raw(&raw const (*self.raw.as_ptr()).format) }
    }

    /// Whether the allocator that backs this swapchain is still alive.
    ///
    /// wlroots nulls the field from its own listener on the allocator's destroy
    /// signal, so this is a fact rather than a cached guess.
    pub fn allocator_alive(&self) -> bool {
        // SAFETY: this value owns a live swapchain.
        !unsafe { (*self.raw.as_ptr()).allocator }.is_null()
    }

    /// Take the next free buffer, allocating one if the slot is empty.
    ///
    /// The returned [`LockedBuffer`] is a **consumer** reference: dropping it
    /// unlocks the buffer, which is what frees the slot for a later acquire.
    ///
    /// # Errors
    ///
    /// [`Error::Destroyed`] if the allocator is gone — reachable only for a
    /// swapchain built on an [`AllocatorRef`], since an [`Allocator`] is
    /// borrowed for the swapchain's whole life. [`Error::Operation`] if all
    /// [`SWAPCHAIN_CAP`] slots are still in flight or the allocation failed;
    /// wlroots reports both as a null return and logs which.
    pub fn acquire(&self) -> Result<LockedBuffer<'_>> {
        if !self.allocator_alive() {
            return Err(Error::Destroyed("wlr_allocator"));
        }
        // SAFETY: this value owns a live swapchain whose allocator is live
        // (checked immediately above, on the same thread, with no wlroots call
        // in between that could destroy it).
        let raw = unsafe { sys::wlr_swapchain_acquire(self.raw.as_ptr()) };
        if raw.is_null() {
            return Err(Error::Operation("wlr_swapchain_acquire"));
        }
        // SAFETY: non-null, and wlroots documents the returned buffer as
        // locked — the consumer reference `LockedBuffer` releases.
        Ok(unsafe { LockedBuffer::from_raw(raw) })
    }

    /// Whether `buffer` is one of this swapchain's own.
    pub fn has_buffer(&self, buffer: &Buffer<'_>) -> bool {
        // SAFETY: this value owns a live swapchain, and the borrow keeps the
        // buffer alive; the call only compares pointers.
        unsafe { sys::wlr_swapchain_has_buffer(self.raw.as_ptr(), buffer.as_ptr()) }
    }

    /// How many slots are acquired and not yet released.
    ///
    /// Computed from the slots rather than remembered, so a buffer released by
    /// something else — wlroots' own output code, say — is reflected.
    pub fn in_flight(&self) -> usize {
        // SAFETY: this value owns a live swapchain; `slots` is a fixed-size
        // array of `WLR_SWAPCHAIN_CAP` entries, which `SWAPCHAIN_CAP` is read
        // from.
        let slots = unsafe { &(*self.raw.as_ptr()).slots };
        slots.iter().filter(|slot| slot.acquired).count()
    }
}

impl std::fmt::Debug for Swapchain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Swapchain")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("in_flight", &self.in_flight())
            .field("allocator_alive", &self.allocator_alive())
            .finish()
    }
}

impl Drop for Swapchain<'_> {
    fn drop(&mut self) {
        // SAFETY: this value owns the swapchain. `wlr_swapchain_destroy` drops
        // the swapchain's own reference on every slot buffer; a buffer some
        // consumer still holds a `LockedBuffer` for survives — and cannot be
        // outstanding here anyway, since a `LockedBuffer` borrows this value.
        unsafe { sys::wlr_swapchain_destroy(self.raw.as_ptr()) };
    }
}

/// A **consumer** reference to a buffer. Dropping it calls
/// `wlr_buffer_unlock`.
///
/// What a swapchain hands out. Not interchangeable with
/// [`OwnedBuffer`](crate::OwnedBuffer), the producer reference an allocator
/// hands out — see `allocator.rs`'s own doc.
pub struct LockedBuffer<'a> {
    buffer: Buffer<'a>,
}

impl<'a> LockedBuffer<'a> {
    /// # Safety
    ///
    /// `raw` must be a live `wlr_buffer` whose **lock** this value takes over.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_buffer) -> LockedBuffer<'a> {
        LockedBuffer {
            // SAFETY: forwarded; the lock this value now holds is what keeps
            // the buffer alive for `'a`.
            buffer: unsafe { Buffer::from_raw(raw) },
        }
    }
}

impl<'a> Deref for LockedBuffer<'a> {
    type Target = Buffer<'a>;

    fn deref(&self) -> &Buffer<'a> {
        &self.buffer
    }
}

impl std::fmt::Debug for LockedBuffer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockedBuffer")
            .field("width", &self.buffer.width())
            .field("height", &self.buffer.height())
            .finish()
    }
}

impl Drop for LockedBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: this value holds one lock and releases it exactly once. The
        // buffer is freed only if this was the last reference and the producer
        // has already dropped its own.
        unsafe { sys::wlr_buffer_unlock(self.buffer.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ring size is wlroots ABI, and `in_flight` reads a fixed-size array
    /// sized by it.
    #[test]
    fn swapchain_cap_matches_wlroots() {
        assert_eq!(SWAPCHAIN_CAP, 4);
        assert_eq!(SWAPCHAIN_CAP as u32, sys::WLR_SWAPCHAIN_CAP);
        assert_eq!(
            SWAPCHAIN_CAP,
            std::mem::size_of::<[sys::wlr_swapchain_slot; SWAPCHAIN_CAP]>()
                / std::mem::size_of::<sys::wlr_swapchain_slot>()
        );
    }
}
