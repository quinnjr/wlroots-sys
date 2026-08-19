//! DMA-BUF attributes: the file descriptors and layout a GPU buffer is
//! described by.
//!
//! # Two types, because there are two owners
//!
//! `wlr_dmabuf_attributes` carries **owned** file descriptors, and
//! `wlr_dmabuf_attributes_finish` closes every one of them. Whether that call is
//! correct depends entirely on where the attributes came from:
//!
//! * [`DmabufAttributes`] — built here from `OwnedFd`s, or cloned from a
//!   borrowed view. `Drop` calls `wlr_dmabuf_attributes_finish`.
//! * [`DmabufAttributesRef`] — what [`Buffer::dmabuf`](crate::Buffer::dmabuf)
//!   returns. wlroots documents those attributes as "valid for the lifetime of
//!   the `struct wlr_buffer`. The caller isn't responsible for cleaning up the
//!   DMA-BUF attributes." **No `Drop`**: finishing one of these closes
//!   descriptors the buffer still uses, and the buffer closes them again later.
//!
//! Neither type ever hands out an `OwnedFd` taken from its own storage. A
//! consumer who genuinely needs one calls
//! [`DmabufAttributes::plane_fd`], which dups.

use std::marker::PhantomData;
use std::os::fd::{BorrowedFd, IntoRawFd, OwnedFd};

use crate::error::{Error, Result};
use crate::sys;

use super::format::{FourCc, Modifier};

/// `WLR_DMABUF_MAX_PLANES`: the most planes one DMA-BUF can have.
pub const DMABUF_MAX_PLANES: usize = sys::WLR_DMABUF_MAX_PLANES as usize;

/// One plane of a DMA-BUF.
///
/// `fd` is borrowed from whichever attributes struct produced it — closing it
/// is that struct's job, not the reader's.
#[derive(Debug, Clone, Copy)]
pub struct DmabufPlane<'a> {
    /// Byte offset of this plane's data within its descriptor.
    pub offset: u32,
    /// Byte stride of one row of this plane.
    pub stride: u32,
    /// The descriptor backing this plane.
    pub fd: BorrowedFd<'a>,
}

/// The read-only surface, shared by [`DmabufAttributes`] and
/// [`DmabufAttributesRef`].
///
/// # Safety
///
/// Every function here requires `raw` to point at a live, fully-initialised
/// `wlr_dmabuf_attributes` whose `n_planes` is in `1..=WLR_DMABUF_MAX_PLANES`.
mod read {
    use super::{DmabufPlane, FourCc, Modifier, sys};
    use std::os::fd::BorrowedFd;

    pub(super) unsafe fn planes<'a>(
        raw: *const sys::wlr_dmabuf_attributes,
    ) -> impl Iterator<Item = DmabufPlane<'a>> {
        // SAFETY: the caller guarantees `raw` is live and its plane count is in
        // range, so every index below `n_planes` names an initialised slot.
        let (n, offsets, strides, fds) = unsafe {
            let r = &*raw;
            (r.n_planes.max(0) as usize, r.offset, r.stride, r.fd)
        };
        (0..n.min(super::DMABUF_MAX_PLANES)).map(move |i| DmabufPlane {
            offset: offsets[i],
            stride: strides[i],
            // SAFETY: the descriptor is owned by the attributes struct, which
            // the caller's borrow keeps alive for `'a`.
            fd: unsafe { BorrowedFd::borrow_raw(fds[i]) },
        })
    }

    pub(super) unsafe fn format(raw: *const sys::wlr_dmabuf_attributes) -> FourCc {
        // SAFETY: as above; a field read.
        FourCc(unsafe { (*raw).format })
    }

    pub(super) unsafe fn modifier(raw: *const sys::wlr_dmabuf_attributes) -> Modifier {
        // SAFETY: as above.
        Modifier(unsafe { (*raw).modifier })
    }
}

/// Whether a plane count wlroots (or a consumer) supplied is one this crate is
/// willing to read.
///
/// wlroots stores the planes in fixed four-element arrays and does not itself
/// re-check `n_planes` on the way out of `wlr_buffer_get_dmabuf`, so a buffer
/// implementation that filled it in wrongly would have this crate index past
/// the end. Validated at every construction from raw instead.
fn plane_count_in_range(n_planes: std::ffi::c_int) -> bool {
    n_planes > 0 && (n_planes as usize) <= DMABUF_MAX_PLANES
}

/// DMA-BUF attributes this crate owns, including their descriptors.
///
/// `Drop` calls `wlr_dmabuf_attributes_finish`, closing every plane's
/// descriptor exactly once.
pub struct DmabufAttributes {
    raw: sys::wlr_dmabuf_attributes,
}

impl DmabufAttributes {
    /// Build attributes from descriptors this call takes ownership of.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] when `planes` is empty or longer than
    /// [`DMABUF_MAX_PLANES`], or when `width`/`height` are not positive. The
    /// descriptors are closed on that path, since this call was given them.
    pub fn new(
        width: i32,
        height: i32,
        format: FourCc,
        modifier: Modifier,
        planes: Vec<(u32, u32, OwnedFd)>,
    ) -> Result<DmabufAttributes> {
        if planes.is_empty() || planes.len() > DMABUF_MAX_PLANES || width <= 0 || height <= 0 {
            // `planes` drops here, closing every descriptor: taking ownership
            // and then losing the fds would be a descriptor leak in the
            // caller's process, and handing them back would need a second
            // return type for a case nobody can act on.
            return Err(Error::Operation("DmabufAttributes::new"));
        }

        let mut raw = sys::wlr_dmabuf_attributes {
            width,
            height,
            format: format.0,
            modifier: modifier.0,
            n_planes: planes.len() as std::ffi::c_int,
            offset: [0; DMABUF_MAX_PLANES],
            stride: [0; DMABUF_MAX_PLANES],
            // -1, not 0: an unused slot must not name descriptor 0, which is a
            // real descriptor in every process.
            fd: [-1; DMABUF_MAX_PLANES],
        };
        for (i, (offset, stride, fd)) in planes.into_iter().enumerate() {
            raw.offset[i] = offset;
            raw.stride[i] = stride;
            // `into_raw_fd` is what transfers ownership into `raw`; from here
            // on the `Drop` below is what closes it.
            raw.fd[i] = fd.into_raw_fd();
        }
        Ok(DmabufAttributes { raw })
    }

    /// `None` when `raw.n_planes` is outside `1..=DMABUF_MAX_PLANES`.
    ///
    /// The borrowing constructor has always rejected that, and
    /// [`plane_count_in_range`]'s own doc claims it is "validated at every
    /// construction from raw" — which this one made false. It matters here
    /// more than there: the accessors clamp with `.min(DMABUF_MAX_PLANES)` and
    /// stay in bounds, but `Drop` calls `wlr_dmabuf_attributes_finish`, which
    /// loops `0..n_planes` with no clamp — so an `n_planes` of 5 reads `fd[4]`
    /// past the end of a four-element array and `close()`s whatever integer is
    /// there. Some unrelated descriptor in the process, chosen by whatever the
    /// struct happens to be followed by.
    ///
    /// Latent today, because the one caller is fed by an already-validated
    /// `Ref`. The safety contract is what the next caller reads, though, and
    /// it did not forbid the case — and a `wlr_buffer_impl` supplied by a
    /// client is exactly where an out-of-range count would come from.
    ///
    /// # Safety
    ///
    /// `raw` must be a fully-initialised `wlr_dmabuf_attributes` whose
    /// descriptors this value may close.
    pub(crate) unsafe fn from_raw(raw: sys::wlr_dmabuf_attributes) -> Option<DmabufAttributes> {
        plane_count_in_range(raw.n_planes).then_some(DmabufAttributes { raw })
    }

    pub(crate) fn as_ptr(&self) -> *const sys::wlr_dmabuf_attributes {
        &raw const self.raw
    }

    /// Buffer width in pixels.
    pub fn width(&self) -> i32 {
        self.raw.width
    }

    /// Buffer height in pixels.
    pub fn height(&self) -> i32 {
        self.raw.height
    }

    /// The pixel format.
    pub fn format(&self) -> FourCc {
        // SAFETY: `raw` is this value's own live, validated attributes.
        unsafe { read::format(self.as_ptr()) }
    }

    /// The layout modifier. See [`Modifier`] for the contract this must obey.
    pub fn modifier(&self) -> Modifier {
        // SAFETY: as above.
        unsafe { read::modifier(self.as_ptr()) }
    }

    /// The planes, with descriptors borrowed from this value.
    pub fn planes(&self) -> impl Iterator<Item = DmabufPlane<'_>> + '_ {
        // SAFETY: as above; the borrow keeps the descriptors open.
        unsafe { read::planes(self.as_ptr()) }
    }

    /// A **new** descriptor for plane `index`, duplicated so the caller owns it.
    ///
    /// Never a descriptor out of this value's own storage: closing that one
    /// would leave `Drop` closing it a second time.
    pub fn plane_fd(&self, index: usize) -> std::io::Result<OwnedFd> {
        let plane = self
            .planes()
            .nth(index)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        plane.fd.try_clone_to_owned()
    }
}

impl std::fmt::Debug for DmabufAttributes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmabufAttributes")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("format", &self.format())
            .field("modifier", &self.modifier())
            .field("n_planes", &self.raw.n_planes)
            .finish()
    }
}

impl Drop for DmabufAttributes {
    fn drop(&mut self) {
        // SAFETY: `raw` is a live attributes struct whose descriptors this
        // value owns — either taken by `new`, or dup'd by
        // `wlr_dmabuf_attributes_copy` in `DmabufAttributesRef::try_clone`.
        // `finish` closes exactly `n_planes` of them and zeroes the struct, so
        // it cannot run twice against the same descriptors.
        unsafe { sys::wlr_dmabuf_attributes_finish(&raw mut self.raw) };
    }
}

/// DMA-BUF attributes wlroots owns.
///
/// Valid for the lifetime of the buffer they came from. **No `Drop`** — see
/// this module's own doc.
#[derive(Clone, Copy)]
pub struct DmabufAttributesRef<'a> {
    /// Held by value rather than by pointer because
    /// `wlr_buffer_get_dmabuf` fills in a caller-supplied struct; the
    /// descriptors inside it are the buffer's, which is what `'a` tracks.
    raw: sys::wlr_dmabuf_attributes,
    _scope: PhantomData<&'a ()>,
}

impl<'a> DmabufAttributesRef<'a> {
    /// # Safety
    ///
    /// `raw` must be a fully-initialised `wlr_dmabuf_attributes` whose
    /// descriptors are owned by an object that outlives `'a`, and which this
    /// value must **not** close.
    pub(crate) unsafe fn from_raw(
        raw: sys::wlr_dmabuf_attributes,
    ) -> Option<DmabufAttributesRef<'a>> {
        if !plane_count_in_range(raw.n_planes) {
            return None;
        }
        Some(DmabufAttributesRef {
            raw,
            _scope: PhantomData,
        })
    }

    /// Buffer width in pixels.
    pub fn width(&self) -> i32 {
        self.raw.width
    }

    /// Buffer height in pixels.
    pub fn height(&self) -> i32 {
        self.raw.height
    }

    /// The pixel format.
    pub fn format(&self) -> FourCc {
        // SAFETY: `raw` is a live copy of the buffer's attributes, validated at
        // construction; the descriptors it names are kept open by `'a`.
        unsafe { read::format(&raw const self.raw) }
    }

    /// The layout modifier. See [`Modifier`] for the contract this must obey.
    pub fn modifier(&self) -> Modifier {
        // SAFETY: as above.
        unsafe { read::modifier(&raw const self.raw) }
    }

    /// The planes, with descriptors borrowed from the owning buffer.
    pub fn planes(&self) -> impl Iterator<Item = DmabufPlane<'_>> + '_ {
        // SAFETY: as above.
        unsafe { read::planes(&raw const self.raw) }
    }

    /// Copy these attributes into ones this crate owns, **dup'ing every
    /// descriptor** (`wlr_dmabuf_attributes_copy`).
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if a `dup` failed; wlroots cleans up whatever it had
    /// already dup'd before reporting.
    pub fn try_clone(&self) -> Result<DmabufAttributes> {
        let mut dst = sys::wlr_dmabuf_attributes {
            width: 0,
            height: 0,
            format: 0,
            modifier: 0,
            n_planes: 0,
            offset: [0; DMABUF_MAX_PLANES],
            stride: [0; DMABUF_MAX_PLANES],
            fd: [-1; DMABUF_MAX_PLANES],
        };
        // SAFETY: `dst` is a live local this call owns exclusively; `self.raw`
        // is a live source that is only read. On success wlroots has dup'd
        // every descriptor into `dst`, which is exactly the ownership
        // `DmabufAttributes::from_raw` then takes on.
        let ok = unsafe { sys::wlr_dmabuf_attributes_copy(&raw mut dst, &raw const self.raw) };
        if !ok {
            return Err(Error::Operation("wlr_dmabuf_attributes_copy"));
        }
        // SAFETY: the descriptors in `dst` are fresh dups this call now owns.
        //
        // The plane-count check inside `from_raw` cannot fail here — `self` is
        // a validated `Ref` and `wlr_dmabuf_attributes_copy` carries the count
        // across unchanged — but going through it rather than around it is the
        // point: the guarantee lives in one place, and a future caller that
        // is *not* already validated inherits it.
        unsafe { DmabufAttributes::from_raw(dst) }.ok_or(Error::Operation(
            "wlr_dmabuf_attributes n_planes out of range",
        ))
    }
}

impl std::fmt::Debug for DmabufAttributesRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmabufAttributesRef")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("format", &self.format())
            .field("modifier", &self.modifier())
            .field("n_planes", &self.raw.n_planes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    /// A descriptor whose closure this module's tests can observe **without
    /// looking at descriptor numbers**.
    ///
    /// The obvious probe — remember the raw number, then ask whether it is
    /// still open — is racy here: `cargo test` runs one binary's unit tests on
    /// several threads, so another test can be handed the very number this one
    /// just closed. A pipe has no such ambiguity: while the write end is open a
    /// non-blocking read of the read end reports "would block", and the moment
    /// the last copy of the write end is closed it reports end-of-file instead.
    struct FdProbe {
        read: OwnedFd,
    }

    impl FdProbe {
        /// The probe, and the write end to hand to whatever is under test.
        fn new() -> (FdProbe, OwnedFd) {
            let (read, write) =
                rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).expect("pipe");
            (FdProbe { read }, write)
        }

        /// Whether every copy of the write end has been closed.
        fn writer_closed(&self) -> bool {
            let mut buf = [0u8; 1];
            match rustix::io::read(&self.read, &mut buf) {
                // End-of-file: nothing holds the write end any more.
                Ok(0) => true,
                // Data (nothing ever writes here) or "would block": still open.
                Ok(_) | Err(_) => false,
            }
        }
    }

    fn attrs_from_owned(fd: OwnedFd) -> Result<DmabufAttributes> {
        DmabufAttributes::new(
            16,
            16,
            FourCc::ARGB8888,
            Modifier::LINEAR,
            vec![(0, 64, fd)],
        )
    }

    #[test]
    fn max_planes_matches_wlroots() {
        assert_eq!(DMABUF_MAX_PLANES, 4);
        assert_eq!(DMABUF_MAX_PLANES as u32, sys::WLR_DMABUF_MAX_PLANES);
    }

    #[test]
    fn attributes_report_what_they_were_built_with() {
        let (_probe, fd) = FdProbe::new();
        let attrs = attrs_from_owned(fd).expect("one plane");
        assert_eq!(attrs.width(), 16);
        assert_eq!(attrs.height(), 16);
        assert_eq!(attrs.format(), FourCc::ARGB8888);
        assert_eq!(attrs.modifier(), Modifier::LINEAR);
        let planes: Vec<_> = attrs.planes().collect();
        assert_eq!(planes.len(), 1);
        assert_eq!(planes[0].stride, 64);
        assert_eq!(planes[0].offset, 0);
    }

    /// The whole point of the owned type: dropping it closes the descriptors it
    /// was given, and closes only those.
    #[test]
    fn dropping_owned_attributes_closes_their_descriptors() {
        let (probe, fd) = FdProbe::new();
        let (untouched, _untouched_write) = FdProbe::new();

        let attrs = attrs_from_owned(fd).expect("one plane");
        assert!(!probe.writer_closed());
        drop(attrs);

        assert!(probe.writer_closed(), "the plane descriptor stayed open");
        assert!(
            !untouched.writer_closed(),
            "an unrelated descriptor was closed"
        );
    }

    /// A borrowed view must close nothing: its descriptors belong to the buffer
    /// it was read out of, which closes them itself.
    #[test]
    fn dropping_a_borrowed_view_closes_nothing() {
        let (probe, write) = FdProbe::new();

        let c = sys::wlr_dmabuf_attributes {
            width: 8,
            height: 8,
            format: FourCc::ARGB8888.0,
            modifier: Modifier::LINEAR.0,
            n_planes: 1,
            offset: [0; DMABUF_MAX_PLANES],
            stride: [32, 0, 0, 0],
            fd: [write.as_raw_fd(), -1, -1, -1],
        };

        // SAFETY: `write` (still owned by this test) outlives the view, and the
        // view never closes it — which is exactly what this test asserts.
        let view = unsafe { DmabufAttributesRef::from_raw(c) }.expect("one plane");
        assert_eq!(view.planes().count(), 1);
        // Nothing to drop: the view is `Copy`, which the compiler already
        // takes as proof that it has no `Drop` impl — a `finish` here would
        // be the double close this type exists to prevent. What is left to
        // check is that reading the attributes did not close anything either.
        let _ = view;

        assert!(
            !probe.writer_closed(),
            "a borrowed view must not close its fds"
        );
        drop(write);
        assert!(probe.writer_closed());
    }

    #[test]
    fn a_plane_count_out_of_range_is_refused_rather_than_read() {
        let c = sys::wlr_dmabuf_attributes {
            width: 8,
            height: 8,
            format: FourCc::ARGB8888.0,
            modifier: Modifier::LINEAR.0,
            n_planes: 5,
            offset: [0; DMABUF_MAX_PLANES],
            stride: [0; DMABUF_MAX_PLANES],
            fd: [-1; DMABUF_MAX_PLANES],
        };
        // SAFETY: nothing is read out of the struct on the rejection path.
        assert!(unsafe { DmabufAttributesRef::from_raw(c) }.is_none());

        let c = sys::wlr_dmabuf_attributes { n_planes: 0, ..c };
        // SAFETY: as above.
        assert!(unsafe { DmabufAttributesRef::from_raw(c) }.is_none());
    }

    #[test]
    fn new_rejects_an_empty_plane_list() {
        assert_eq!(
            DmabufAttributes::new(4, 4, FourCc::ARGB8888, Modifier::LINEAR, Vec::new()).err(),
            Some(Error::Operation("DmabufAttributes::new"))
        );
    }

    /// A rejected construction was still handed the descriptors, so it must
    /// close them rather than leak ones the caller can no longer name.
    #[test]
    fn new_closes_the_descriptors_it_rejects() {
        let mut probes = Vec::new();
        let mut planes = Vec::new();
        for _ in 0..DMABUF_MAX_PLANES + 1 {
            let (probe, write) = FdProbe::new();
            probes.push(probe);
            planes.push((0, 16, write));
        }

        assert_eq!(
            DmabufAttributes::new(4, 4, FourCc::ARGB8888, Modifier::LINEAR, planes).err(),
            Some(Error::Operation("DmabufAttributes::new"))
        );
        for probe in &probes {
            assert!(probe.writer_closed());
        }
    }

    /// `plane_fd` dups: the returned descriptor must survive the attributes it
    /// came from.
    #[test]
    fn plane_fd_returns_a_dup_not_the_stored_descriptor() {
        let (probe, fd) = FdProbe::new();
        let attrs = attrs_from_owned(fd).expect("one plane");
        let stored = attrs.planes().next().unwrap().fd.as_raw_fd();
        let owned = attrs.plane_fd(0).expect("dup");
        assert_ne!(owned.as_raw_fd(), stored);

        drop(attrs);
        assert!(
            !probe.writer_closed(),
            "the dup should still hold the pipe open"
        );
        drop(owned);
        assert!(probe.writer_closed());
    }

    #[test]
    fn plane_fd_refuses_an_index_past_the_end() {
        let (_probe, fd) = FdProbe::new();
        let attrs = attrs_from_owned(fd).expect("one plane");
        assert!(attrs.plane_fd(1).is_err());
    }

    /// `try_clone` goes through `wlr_dmabuf_attributes_copy`, which dups every
    /// descriptor — so the clone outlives the source and closes its own.
    #[test]
    fn try_clone_dups_every_descriptor() {
        let (probe, write) = FdProbe::new();
        let c = sys::wlr_dmabuf_attributes {
            width: 8,
            height: 8,
            format: FourCc::ARGB8888.0,
            modifier: Modifier::LINEAR.0,
            n_planes: 1,
            offset: [0; DMABUF_MAX_PLANES],
            stride: [32, 0, 0, 0],
            fd: [write.as_raw_fd(), -1, -1, -1],
        };
        // SAFETY: `write` outlives the view and the view closes nothing.
        let view = unsafe { DmabufAttributesRef::from_raw(c) }.expect("one plane");

        let cloned = view.try_clone().expect("dup succeeds");
        assert_ne!(
            cloned.planes().next().unwrap().fd.as_raw_fd(),
            write.as_raw_fd()
        );

        drop(write);
        assert!(
            !probe.writer_closed(),
            "the clone owns its own copy of the descriptor"
        );
        drop(cloned);
        assert!(probe.writer_closed());
    }
}
