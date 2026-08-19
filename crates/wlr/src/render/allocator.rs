//! Allocators: where a compositor's own buffers come from.
//!
//! wlroots picks an implementation (gbm, udmabuf, shm, DRM dumb) from what the
//! backend and the renderer can both deal in. What comes back is
//! [`Allocator`] — owned, with a `Drop` — and the buffers it hands out are
//! [`OwnedBuffer`]s, the **producer** half of `wlr_buffer`'s two-role refcount.
//!
//! # Producer and consumer are not interchangeable
//!
//! `wlr_allocator_create_buffer` returns a buffer the caller must release with
//! `wlr_buffer_drop`; `wlr_swapchain_acquire` returns one the caller must
//! release with `wlr_buffer_unlock`. Calling the wrong one is a leak in one
//! direction and a premature free in the other, and neither is diagnosed. That
//! is why they are different Rust types — [`OwnedBuffer`] here and
//! [`LockedBuffer`](crate::LockedBuffer) in `swapchain.rs` — that both deref to
//! the read-only [`Buffer`], and why neither can be converted into the other.
//! `crates/wlr/src/buffer.rs`'s "Refcount story" is the long version.

use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;

use crate::backend::Backend;
use crate::buffer::Buffer;
use crate::error::{Error, Result};
use crate::sys;

use super::format::DrmFormat;
use super::{BufferCaps, Renderer};

/// An allocator this crate owns. Dropping it calls `wlr_allocator_destroy`.
///
/// Borrows the [`Renderer`] it was created against. wlroots does not strictly
/// require the renderer to outlive the allocator for every implementation, but
/// tying them costs nothing and removes a whole class of ordering question.
pub struct Allocator<'r> {
    raw: NonNull<sys::wlr_allocator>,
    _renderer: PhantomData<&'r ()>,
}

impl<'r> Allocator<'r> {
    /// Create the allocator wlroots picks for `backend` and `renderer`.
    ///
    /// # Errors
    ///
    /// [`Error::Destroyed`] if the backend is already gone, [`Error::Create`] if
    /// no implementation matched what the backend and renderer can both use.
    pub fn autocreate(backend: &Backend<'_>, renderer: &'r Renderer) -> Result<Allocator<'r>> {
        backend.alive_or_err()?;
        // SAFETY: the check above proves the backend is live, and `renderer`
        // owns a live renderer for at least `'r`. wlroots reads both and stores
        // neither (the chosen implementation keeps its own DRM node, opened
        // fresh — `render/allocator/allocator.c`).
        let raw = unsafe { sys::wlr_allocator_autocreate(backend.as_ptr(), renderer.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_allocator_autocreate"))?;
        Ok(Allocator {
            raw,
            _renderer: PhantomData,
        })
    }

    /// The raw allocator, for interop with code that speaks `wlr-sys` directly.
    ///
    /// Borrowed, never owned.
    pub fn as_ptr(&self) -> *mut sys::wlr_allocator {
        self.raw.as_ptr()
    }

    /// A non-owning view of an allocator this crate did not create.
    ///
    /// # Safety
    ///
    /// `raw` must point at a live `wlr_allocator` that this call does **not**
    /// take ownership of, and the view must not be *used* after that allocator
    /// is destroyed. A [`Swapchain`](crate::Swapchain) built on the view is
    /// exempt from the second half: it holds no Rust-side pointer to the
    /// allocator, and wlroots nulls its own
    /// (`swapchain_handle_allocator_destroy`), which is what
    /// [`Swapchain::acquire`](crate::Swapchain::acquire) reports as
    /// [`Error::Destroyed`].
    pub unsafe fn borrow_from_ptr<'a>(raw: *mut sys::wlr_allocator) -> AllocatorRef<'a> {
        // SAFETY: forwarded verbatim to the constructor's own contract.
        unsafe { AllocatorRef::from_raw(raw) }
    }

    /// The kinds of buffer this allocator produces.
    pub fn buffer_caps(&self) -> BufferCaps {
        // SAFETY: this value owns a live allocator for as long as it exists.
        BufferCaps::from_bits(unsafe { (*self.raw.as_ptr()).buffer_caps })
    }

    /// Allocate a buffer.
    ///
    /// The returned [`OwnedBuffer`] is a **producer** reference: dropping it
    /// calls `wlr_buffer_drop`, which frees the buffer unless a consumer (a
    /// texture, a scene node) has locked it in the meantime.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] for a non-positive width or height — wlroots takes
    /// them as `int` and does not check, and every implementation multiplies
    /// them out. [`Error::Create`] if the allocation itself failed, which
    /// includes the allocator refusing the format.
    pub fn create_buffer(
        &self,
        width: i32,
        height: i32,
        format: &DrmFormat,
    ) -> Result<OwnedBuffer<'_>> {
        if width <= 0 || height <= 0 {
            return Err(Error::Operation("Allocator::create_buffer"));
        }
        // SAFETY: this value owns a live allocator; `with_sys` keeps the
        // temporary format (and the modifier array it borrows) alive for the
        // whole call, which is all wlroots needs — it copies whatever it keeps.
        let raw = format.with_sys(|fmt| unsafe {
            sys::wlr_allocator_create_buffer(self.raw.as_ptr(), width, height, fmt)
        });
        if raw.is_null() {
            return Err(Error::Create("wlr_allocator_create_buffer"));
        }
        // SAFETY: non-null, and this is the producer reference wlroots just
        // handed us — exactly what `OwnedBuffer` releases with
        // `wlr_buffer_drop`.
        Ok(unsafe { OwnedBuffer::from_raw(raw) })
    }
}

impl std::fmt::Debug for Allocator<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Allocator")
            .field("buffer_caps", &self.buffer_caps())
            .finish()
    }
}

impl Drop for Allocator<'_> {
    fn drop(&mut self) {
        // SAFETY: this value owns the allocator — wlroots destroys only the one
        // the DRM backend creates for itself — so this is the one and only
        // destroy. Buffers already handed out are independently refcounted and
        // survive it.
        //
        // No listener of this crate's own is linked into `events.destroy`, and
        // that is deliberate: `wlr_allocator_destroy` **asserts** the signal's
        // listener list is empty once it has emitted (`render/allocator/
        // allocator.c`), and that assertion is compiled into the distro's
        // library. wlroots' own `wlr_swapchain` listener is fine — it unlinks
        // itself from inside the emission — but one of ours would have to do
        // the same, and there is nothing for it to report: a `Swapchain`
        // borrows its `Allocator`, so an allocator cannot be destroyed while
        // one is alive, and `wlr_swapchain.allocator` is a checkable field for
        // the views that do not borrow.
        unsafe { sys::wlr_allocator_destroy(self.raw.as_ptr()) };
    }
}

/// An allocator this crate does **not** own — the one
/// [`Runtime::init_graphics`](crate::Runtime::init_graphics) created, reached
/// through [`Runtime::allocator_ref`](crate::Runtime::allocator_ref).
///
/// The same surface as [`Allocator`] minus anything that could free it.
#[derive(Clone, Copy)]
pub struct AllocatorRef<'a> {
    raw: NonNull<sys::wlr_allocator>,
    _scope: PhantomData<&'a ()>,
}

impl<'a> AllocatorRef<'a> {
    /// # Safety
    ///
    /// `raw` must point at a live `wlr_allocator` that outlives `'a` and that
    /// this value must not free.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_allocator) -> AllocatorRef<'a> {
        AllocatorRef {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_allocator"),
            _scope: PhantomData,
        }
    }

    /// The raw allocator, for interop. Borrowed.
    pub fn as_ptr(&self) -> *mut sys::wlr_allocator {
        self.raw.as_ptr()
    }

    /// The kinds of buffer this allocator produces.
    pub fn buffer_caps(&self) -> BufferCaps {
        // SAFETY: `'a` guarantees the allocator is live.
        BufferCaps::from_bits(unsafe { (*self.raw.as_ptr()).buffer_caps })
    }

    /// Allocate a buffer. See [`Allocator::create_buffer`].
    ///
    /// # Errors
    ///
    /// As [`Allocator::create_buffer`].
    pub fn create_buffer(
        &self,
        width: i32,
        height: i32,
        format: &DrmFormat,
    ) -> Result<OwnedBuffer<'a>> {
        if width <= 0 || height <= 0 {
            return Err(Error::Operation("Allocator::create_buffer"));
        }
        // SAFETY: `'a` guarantees the allocator is live; the temporary format
        // outlives the call.
        let raw = format.with_sys(|fmt| unsafe {
            sys::wlr_allocator_create_buffer(self.raw.as_ptr(), width, height, fmt)
        });
        if raw.is_null() {
            return Err(Error::Create("wlr_allocator_create_buffer"));
        }
        // SAFETY: as in `Allocator::create_buffer`.
        Ok(unsafe { OwnedBuffer::from_raw(raw) })
    }
}

impl std::fmt::Debug for AllocatorRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AllocatorRef")
            .field("buffer_caps", &self.buffer_caps())
            .finish()
    }
}

/// A **producer** reference to a buffer. Dropping it calls `wlr_buffer_drop`.
///
/// What an allocator hands out. Not interchangeable with
/// [`LockedBuffer`](crate::LockedBuffer), the consumer reference a swapchain
/// hands out — see this module's own doc.
pub struct OwnedBuffer<'a> {
    buffer: Buffer<'a>,
}

impl<'a> OwnedBuffer<'a> {
    /// # Safety
    ///
    /// `raw` must be a live `wlr_buffer` whose **producer** reference this
    /// value takes over.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_buffer) -> OwnedBuffer<'a> {
        OwnedBuffer {
            // SAFETY: forwarded; the producer reference this value now holds is
            // what keeps the buffer alive for `'a`.
            buffer: unsafe { Buffer::from_raw(raw) },
        }
    }
}

impl<'a> Deref for OwnedBuffer<'a> {
    type Target = Buffer<'a>;

    fn deref(&self) -> &Buffer<'a> {
        &self.buffer
    }
}

impl std::fmt::Debug for OwnedBuffer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedBuffer")
            .field("width", &self.buffer.width())
            .field("height", &self.buffer.height())
            .finish()
    }
}

impl Drop for OwnedBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: this value holds the producer reference and releases it
        // exactly once. `wlr_buffer_drop` frees the buffer only if nothing else
        // has locked it; a texture or scene node that did keeps it alive.
        unsafe { sys::wlr_buffer_drop(self.buffer.as_ptr()) };
    }
}
