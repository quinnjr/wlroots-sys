//! Shared-memory attributes for a buffer.
//!
//! Exactly the DMA-BUF story in `dmabuf.rs`, cut down to the one shape that
//! exists here: `wlr_buffer_get_shm` documents its output as "valid for the
//! lifetime of the struct wlr_buffer. The caller isn't responsible for
//! cleaning up the shared memory attributes." — so, like
//! [`DmabufAttributesRef`](super::DmabufAttributesRef), [`ShmAttributesRef`]
//! carries **no `Drop`** and its one descriptor is borrowed, never owned:
//! closing it here would close a descriptor the buffer still uses.
//!
//! There is no owning counterpart the way `DmabufAttributes` exists for
//! DMA-BUFs — nothing in this crate ever builds a `wlr_shm_attributes` to hand
//! to wlroots, only reads one wlroots already owns, so only the borrowed half
//! of that pair is needed.

use std::marker::PhantomData;
use std::os::fd::BorrowedFd;

use crate::sys;

use super::format::FourCc;

/// Shared-memory attributes borrowed from a buffer: the file descriptor of
/// the POSIX shared-memory backing, the pixel format, dimensions, stride and
/// byte offset of the first pixel.
///
/// Valid for the lifetime of the buffer they came from. **No `Drop`** — see
/// this module's own doc.
#[derive(Clone, Copy)]
pub struct ShmAttributesRef<'a> {
    raw: sys::wlr_shm_attributes,
    _scope: PhantomData<&'a ()>,
}

impl<'a> ShmAttributesRef<'a> {
    /// # Safety
    ///
    /// `raw` must be a fully-initialised `wlr_shm_attributes` whose
    /// descriptor is owned by an object that outlives `'a`, and which this
    /// value must **not** close.
    pub(crate) unsafe fn from_raw(raw: sys::wlr_shm_attributes) -> ShmAttributesRef<'a> {
        ShmAttributesRef {
            raw,
            _scope: PhantomData,
        }
    }

    /// The descriptor backing this buffer's shared memory.
    ///
    /// Borrowed, not owned: the buffer closes it, not the caller.
    pub fn fd(&self) -> BorrowedFd<'a> {
        // SAFETY: the descriptor is owned by the buffer this value borrows
        // from, which outlives `'a` by this type's own construction contract.
        unsafe { BorrowedFd::borrow_raw(self.raw.fd) }
    }

    /// The pixel format (a DRM fourcc code).
    pub fn format(&self) -> FourCc {
        FourCc(self.raw.format)
    }

    /// Buffer width in pixels.
    pub fn width(&self) -> i32 {
        self.raw.width
    }

    /// Buffer height in pixels.
    pub fn height(&self) -> i32 {
        self.raw.height
    }

    /// Byte stride of one row.
    pub fn stride(&self) -> i32 {
        self.raw.stride
    }

    /// Byte offset of the first pixel within the descriptor.
    pub fn offset(&self) -> i64 {
        self.raw.offset
    }
}

impl std::fmt::Debug for ShmAttributesRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmAttributesRef")
            .field("format", &self.format())
            .field("width", &self.width())
            .field("height", &self.height())
            .field("stride", &self.stride())
            .field("offset", &self.offset())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    fn attrs(fd: std::os::fd::RawFd) -> sys::wlr_shm_attributes {
        sys::wlr_shm_attributes {
            fd,
            format: FourCc::ARGB8888.0,
            width: 4,
            height: 2,
            stride: 16,
            offset: 0,
        }
    }

    #[test]
    fn reads_every_field_back() {
        // A real, harmless descriptor: stdin. Never closed by this test or by
        // `ShmAttributesRef`, which is exactly the point.
        let view = unsafe { ShmAttributesRef::from_raw(attrs(0)) };
        assert_eq!(view.format(), FourCc::ARGB8888);
        assert_eq!(view.width(), 4);
        assert_eq!(view.height(), 2);
        assert_eq!(view.stride(), 16);
        assert_eq!(view.offset(), 0);
        assert_eq!(view.fd().as_raw_fd(), 0);
    }
}
