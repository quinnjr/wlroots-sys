//! Renderers, textures, render passes, allocators and swapchains.
//!
//! # Who owns what
//!
//! This module is the exception to the crate's usual answer that wlroots owns
//! everything. A renderer, an allocator, a swapchain, a texture and a render
//! pass are all created *by* the compositor and freed *by* the compositor;
//! wlroots never destroys one behind your back. Verified rather than assumed:
//! the only `wlr_renderer_destroy`/`wlr_allocator_destroy` calls inside wlroots
//! 0.20.2 are in `backend/drm/renderer.c`, tearing down the multi-GPU
//! renderer the DRM backend created for itself, and in `tinywl`. So these
//! types have real `Drop` impls, and there is no `Error::Destroyed` among their
//! failures: nothing can have destroyed them.
//!
//! The ordering rules that ownership implies are carried by borrows rather than
//! by documentation:
//!
//! | type | released by | outlives |
//! |---|---|---|
//! | [`Renderer`] | `wlr_renderer_destroy` | its textures, timers, allocators, passes |
//! | [`Texture`] | `wlr_texture_destroy` | — |
//! | [`RenderTimer`] | `wlr_render_timer_destroy` | the pass that names it |
//! | [`RenderPass`] | `wlr_render_pass_submit`, and nothing else | — |
//! | [`Allocator`] | `wlr_allocator_destroy` | its swapchains and buffers |
//! | [`OwnedBuffer`] | `wlr_buffer_drop` (**producer**) | — |
//! | [`LockedBuffer`] | `wlr_buffer_unlock` (**consumer**) | — |
//! | [`Swapchain`] | `wlr_swapchain_destroy` | — |
//! | [`ColorTransform`] | `wlr_color_transform_unref` (**refcounted**) | — |
//! | [`SyncTimeline`] | `wlr_drm_syncobj_timeline_unref` (**refcounted**) | its waiters |
//! | [`SyncWaiter`] | `wlr_drm_syncobj_timeline_waiter_finish` | — |
//! | [`Egl`] | nothing — see below | — |
//!
//! The producer/consumer split in the `OwnedBuffer`/`LockedBuffer`/`Swapchain`
//! rows is the one to get right; `crates/wlr/src/buffer.rs`'s "Refcount story"
//! is the long version.
//!
//! Two rows are not like the others. [`ColorTransform`] and [`SyncTimeline`]
//! are reference-counted by wlroots, so `Clone` on them is a genuine second
//! reference and neither borrows anything — a colour transform may outlive
//! every renderer that ever applied it. [`Egl`] has **no** release path at all
//! except being consumed by
//! [`Renderer::gles2_from_egl`](Renderer::gles2_from_egl); wlroots ships no
//! `wlr_egl_destroy`, so one that is never handed to a renderer leaks. That is
//! wlroots' API, not an omission here.
//!
//! # Backend-specific surfaces
//!
//! wlroots' GLES2, Vulkan and pixman accessors all share one hazard: each is
//! undefined behaviour on a renderer or texture of the wrong kind, and each
//! ships with a separate `wlr_*_is_*` test the caller is trusted to have run.
//! [`Renderer::as_pixman`], `Renderer::as_gles2` and `Renderer::as_vulkan` turn
//! that test into a value — [`Pixman`], `Gles2`, `Vk` — so the precondition
//! cannot be skipped. `pixman.rs` has the long version.
//!
//! The GLES2 and Vulkan halves are `#[cfg]`-gated on `wlr_has_gles2_renderer`
//! and `wlr_has_vulkan_renderer`, which is why they are named here in plain
//! code spans rather than linked: a doc link to an item a smaller feature set
//! does not build is a hard error under `-D warnings`.
//!
//! # Owned versus borrowed renderers
//!
//! [`Runtime::init_graphics`](crate::Runtime::init_graphics) creates a renderer
//! and an allocator of its own and deliberately never frees them (see
//! `Graphics`' own docs). Those are reachable as [`RendererRef`] and
//! [`AllocatorRef`] — the same read-only and texture-creating surface, with no
//! `Drop` and no way to acquire one. A [`Renderer`] built here is a separate
//! object that this crate does destroy. Mixing the two up would be a double
//! free, so they are different types rather than a flag.
//!
//! # The asserts this module exists to keep you away from
//!
//! Arch (and every distro packaging wlroots 0.20) builds without `NDEBUG`, so
//! wlroots' `assert`s are live code that aborts the process. Four of them are
//! reachable from an obvious safe call and are checked here instead:
//!
//! * `wlr_renderer_destroy` asserts both its signal listener lists are empty —
//!   so [`Renderer`] unlinks its own `lost` watch *before* destroying.
//! * `wlr_allocator_destroy` asserts the same of its destroy signal.
//! * `wlr_texture_from_pixels` asserts non-zero width, height and stride and a
//!   non-null pointer.
//! * `wlr_swapchain_create` asserts positive dimensions;
//!   `wlr_drm_format_set_add` asserts a non-`INVALID` format;
//!   `wlr_render_pass_add_rect` asserts non-negative extents;
//!   `wlr_render_pass_add_texture` asserts the source box lies inside the
//!   texture.
//!
//! # Answers recorded for the rest of the render surface
//!
//! Three ownership questions that the headers do not settle, resolved against
//! wlroots 0.20.2's own sources so the wrappers that need them do not have to
//! guess:
//!
//! * `wlr_texture_from_dmabuf` does **not** take the caller's file descriptors.
//!   It wraps them in a `wlr_dmabuf_buffer` and immediately drops it; if a
//!   renderer took a lock, `dmabuf_buffer_drop` *dups* every descriptor into a
//!   saved copy the buffer then owns (`types/buffer/dmabuf.c`). Either way the
//!   caller still owns — and must still close — its own.
//! * `wlr_gles2_renderer_create_with_drm_fd` and
//!   `wlr_vk_renderer_create_with_drm_fd` **borrow** the descriptor: GLES2 uses
//!   it only to name a device and opens its own render node
//!   (`render/egl.c: open_render_node`), Vulkan uses it only to match a
//!   physical device (`render/vulkan/renderer.c`). Neither dups it and neither
//!   closes it, so those take a `BorrowedFd` when they are wrapped.
//! * `wlr_drm_syncobj_timeline_create` **retains** the descriptor: it is stored
//!   on the timeline and used for every later ioctl, destroy included
//!   (`render/drm_syncobj.c`), so it must outlive the timeline.

mod allocator;
mod color;
mod dmabuf;
mod egl;
mod format;
#[cfg(wlr_has_gles2_renderer)]
mod gles2;
mod pass;
pub(crate) mod pixel_format;
mod pixman;
mod shm;
mod swapchain;
mod sync;
mod texture;
#[cfg(wlr_has_vulkan_renderer)]
mod vulkan;

use std::cell::Cell;
// `AsRawFd` is reached only by the two gated `*_with_drm_fd` constructors
// below; without either subsystem the import would be dead.
#[cfg(any(wlr_has_gles2_renderer, wlr_has_vulkan_renderer))]
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::ptr::NonNull;
use std::rc::Rc;

use crate::backend::{Backend, Registration};
use crate::buffer::Buffer;
use crate::display::Display;
use crate::error::{Error, Result};
use crate::sys;

pub use allocator::{Allocator, AllocatorRef, OwnedBuffer};
pub use color::{
    AlphaMode, ChromaLocation, Cie1931Xy, ColorEncoding, ColorEncodings, ColorLuminances,
    ColorPrimaries, ColorRange, ColorTransform, NamedPrimaries, TransferFunction,
    TransferFunctions,
};
pub use dmabuf::{DMABUF_MAX_PLANES, DmabufAttributes, DmabufAttributesRef, DmabufPlane};
pub use egl::Egl;
pub use format::{DrmFormat, DrmFormatRef, DrmFormatSet, DrmFormatSetRef, FourCc, Modifier};
#[cfg(wlr_has_gles2_renderer)]
pub use gles2::{Gles2, Gles2TextureAttribs};
pub use pass::{
    BlendMode, BufferPassOptions, FilterMode, RectOptions, RenderColor, RenderPass, RenderTimer,
    TextureOptions,
};
pub use pixman::Pixman;
pub use shm::ShmAttributesRef;
pub use swapchain::{LockedBuffer, SWAPCHAIN_CAP, Swapchain};
pub use sync::{SyncFlags, SyncTimeline, SyncWaiter};
pub use texture::{ReadPixels, Texture};
#[cfg(wlr_has_vulkan_renderer)]
pub use vulkan::{Vk, VkImageAttribs};

/// The kinds of buffer a renderer or allocator can deal in.
///
/// A bitmask of `enum wlr_buffer_cap`. Hand-rolled rather than a `bitflags`
/// dependency, following [`Interest`](crate::Interest); the three bits are the
/// whole domain and their values are pinned by this module's tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferCaps(u32);

impl BufferCaps {
    /// Buffers whose pixels can be mapped into the process (`wl_shm` clients,
    /// the pixman renderer's own buffers).
    pub const DATA_PTR: BufferCaps = BufferCaps(sys::wlr_buffer_cap::WLR_BUFFER_CAP_DATA_PTR.0);
    /// Buffers backed by a DMA-BUF.
    pub const DMABUF: BufferCaps = BufferCaps(sys::wlr_buffer_cap::WLR_BUFFER_CAP_DMABUF.0);
    /// Buffers backed by POSIX shared memory.
    pub const SHM: BufferCaps = BufferCaps(sys::wlr_buffer_cap::WLR_BUFFER_CAP_SHM.0);

    /// The empty set.
    pub const NONE: BufferCaps = BufferCaps(0);

    /// Build a set from a raw `wlr_buffer_cap` mask.
    pub fn from_bits(bits: u32) -> BufferCaps {
        BufferCaps(bits)
    }

    /// The raw mask.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Whether every bit of `other` is set here.
    pub fn contains(self, other: BufferCaps) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no bit is set.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for BufferCaps {
    type Output = BufferCaps;

    fn bitor(self, rhs: BufferCaps) -> BufferCaps {
        BufferCaps(self.0 | rhs.0)
    }
}

/// What a renderer can do beyond drawing.
///
/// Read from `wlr_renderer.features`, which wlroots fills in at creation and
/// never changes afterwards — except that `WLR_RENDER_NO_EXPLICIT_SYNC=1` in
/// the environment turns `timeline` off at creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendererFeatures {
    /// Colour transforms can be applied to input textures.
    pub input_color_transform: bool,
    /// Colour transforms can be applied to the pass's output.
    pub output_color_transform: bool,
    /// DRM sync-object timelines can be waited on and signalled.
    pub timeline: bool,
}

/// The surface both [`Renderer`] and [`RendererRef`] expose, written once
/// against a raw pointer so the owned and borrowed forms cannot drift apart.
///
/// Duplicated deliberately rather than shared through `Deref`: a `Deref` from
/// the borrowed view to the owned type would hand out `&Renderer`, and with it
/// a path to a destructor for an object the view does not own.
///
/// # Safety
///
/// Every function here requires `raw` to point at a live `wlr_renderer`.
mod imp {
    use super::{
        BufferCaps, ColorEncodings, DrmFormatSetRef, Error, RendererFeatures, Result, sys,
    };
    use std::os::fd::BorrowedFd;

    pub(super) unsafe fn color_encodings(raw: *mut sys::wlr_renderer) -> ColorEncodings {
        // SAFETY: the caller guarantees `raw` is live; this is a plain field
        // read of a mask wlroots fills in at creation and never changes.
        ColorEncodings::from_bits(unsafe { (*raw).color_encodings })
    }

    pub(super) unsafe fn features(raw: *mut sys::wlr_renderer) -> RendererFeatures {
        // SAFETY: the caller guarantees `raw` is live; these are plain field
        // reads of a struct wlroots does not mutate after creation.
        let f = unsafe { (*raw).features };
        RendererFeatures {
            input_color_transform: f.input_color_transform,
            output_color_transform: f.output_color_transform,
            timeline: f.timeline,
        }
    }

    pub(super) unsafe fn buffer_caps(raw: *mut sys::wlr_renderer) -> BufferCaps {
        // SAFETY: as above.
        BufferCaps(unsafe { (*raw).render_buffer_caps })
    }

    pub(super) unsafe fn texture_formats<'a>(
        raw: *mut sys::wlr_renderer,
        caps: BufferCaps,
    ) -> Option<DrmFormatSetRef<'a>> {
        // SAFETY: as above. The returned set is owned by the renderer and valid
        // until it is destroyed, which the caller's borrow outlives.
        let set = unsafe { sys::wlr_renderer_get_texture_formats(raw, caps.bits()) };
        if set.is_null() {
            return None;
        }
        // SAFETY: non-null, owned by the live renderer.
        Some(unsafe { DrmFormatSetRef::from_raw(set) })
    }

    pub(super) unsafe fn drm_fd<'a>(raw: *mut sys::wlr_renderer) -> Option<BorrowedFd<'a>> {
        // SAFETY: as above.
        let fd = unsafe { sys::wlr_renderer_get_drm_fd(raw) };
        if fd < 0 {
            return None;
        }
        // SAFETY: wlroots documents this descriptor as owned by the renderer —
        // "the caller doesn't have ownership of the FD, it must not close it" —
        // so it stays open for as long as the renderer does, which `'a` is
        // bound to at every call site.
        Some(unsafe { BorrowedFd::borrow_raw(fd) })
    }

    pub(super) unsafe fn init_wl_display(
        raw: *mut sys::wlr_renderer,
        display: *mut sys::wl_display,
    ) -> Result<()> {
        // SAFETY: as above, plus a live display from the caller's borrow.
        if unsafe { sys::wlr_renderer_init_wl_display(raw, display) } {
            Ok(())
        } else {
            Err(Error::Operation("wlr_renderer_init_wl_display"))
        }
    }

    pub(super) unsafe fn init_wl_shm(
        raw: *mut sys::wlr_renderer,
        display: *mut sys::wl_display,
    ) -> Result<()> {
        // SAFETY: as above.
        if unsafe { sys::wlr_renderer_init_wl_shm(raw, display) } {
            Ok(())
        } else {
            Err(Error::Operation("wlr_renderer_init_wl_shm"))
        }
    }
}

/// A renderer this crate owns. Dropping it calls `wlr_renderer_destroy`.
///
/// Everything derived from it — [`Texture`]s, [`RenderTimer`]s, [`Allocator`]s,
/// [`RenderPass`]es — borrows it, which is how "textures must be destroyed
/// separately" stops being a rule you have to remember.
pub struct Renderer {
    raw: NonNull<sys::wlr_renderer>,

    /// Set by the `events.lost` watch below. `Rc` rather than `Box` because a
    /// raw pointer to it is handed to a listener while this value is still
    /// movable — the same reason `Backend::alive` is an `Rc`.
    lost: Rc<Cell<bool>>,

    /// The `events.lost` registration.
    ///
    /// `Option` so `Drop` can unlink it *before* `wlr_renderer_destroy`, which
    /// asserts that both of the renderer's signal listener lists are empty
    /// after it emits `destroy` (`render/wlr_renderer.c`). Leaving it to the
    /// field's own drop order would run the unlink after that assertion had
    /// already aborted the process.
    lost_watch: Option<Registration>,

    /// Whether a [`RenderPass`] begun on this renderer is still live. See
    /// `pass.rs`'s module doc for why there may only be one.
    pass_live: Cell<bool>,
}

impl Renderer {
    /// Create the renderer wlroots picks for `backend`.
    ///
    /// Honours `WLR_RENDERER` (`auto`, `gles2`, `vulkan`, `pixman`) and
    /// `WLR_RENDER_DRM_DEVICE`, exactly as
    /// [`Runtime::init_graphics`](crate::Runtime::init_graphics)'s own renderer
    /// does.
    ///
    /// # Errors
    ///
    /// [`Error::Destroyed`] if the backend is already gone, [`Error::Create`] if
    /// wlroots could not initialise any renderer.
    pub fn autocreate(backend: &Backend<'_>) -> Result<Renderer> {
        backend.alive_or_err()?;
        // SAFETY: the check above proves the backend is live, and this call
        // neither stores nor frees it.
        let raw = unsafe { sys::wlr_renderer_autocreate(backend.as_ptr()) };
        Renderer::adopt(raw, "wlr_renderer_autocreate")
    }

    /// Create a CPU renderer.
    ///
    /// Needs no backend and no GPU, which is what makes it the renderer to
    /// reach for in tests. Its buffer capability is
    /// [`BufferCaps::DATA_PTR`] only.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if the allocation failed.
    pub fn pixman() -> Result<Renderer> {
        // SAFETY: takes no arguments and touches nothing this crate owns.
        let raw = unsafe { sys::wlr_pixman_renderer_create() };
        Renderer::adopt(raw, "wlr_pixman_renderer_create")
    }

    /// Wrap a freshly created renderer, or report `what` as having failed.
    fn adopt(raw: *mut sys::wlr_renderer, what: &'static str) -> Result<Renderer> {
        let raw = NonNull::new(raw).ok_or(Error::Create(what))?;
        let lost = Rc::new(Cell::new(false));
        // SAFETY: `raw` is a live renderer whose `events.lost` is an
        // initialised signal (`wlr_renderer_init` does both). The flag is
        // heap-allocated behind an `Rc`, so moving the `Renderer` returned
        // below neither moves it nor re-tags it, and `Drop` unlinks this
        // registration while the renderer — and the flag — are both still
        // alive.
        let lost_watch = unsafe {
            Registration::link_flag(&raw mut (*raw.as_ptr()).events.lost, &raw const *lost)
        };
        Ok(Renderer {
            raw,
            lost,
            lost_watch: Some(lost_watch),
            pass_live: Cell::new(false),
        })
    }

    /// The raw renderer, for interop with code that speaks `wlr-sys` directly.
    ///
    /// Borrowed, never owned: destroying it through this pointer leaves this
    /// value's `Drop` destroying it again.
    pub fn as_ptr(&self) -> *mut sys::wlr_renderer {
        self.raw.as_ptr()
    }

    /// A non-owning view of a renderer this crate did not create.
    ///
    /// # Safety
    ///
    /// `raw` must point at a live `wlr_renderer` that outlives `'a`, and that
    /// this call does **not** take ownership of.
    pub unsafe fn borrow_from_ptr<'a>(raw: *mut sys::wlr_renderer) -> RendererRef<'a> {
        // SAFETY: forwarded verbatim to the constructor's own contract.
        unsafe { RendererRef::from_raw(raw) }
    }

    /// What this renderer supports beyond drawing.
    pub fn features(&self) -> RendererFeatures {
        // SAFETY: this value owns a live renderer for as long as it exists.
        unsafe { imp::features(self.raw.as_ptr()) }
    }

    /// The kinds of buffer this renderer can render *into*.
    pub fn buffer_caps(&self) -> BufferCaps {
        // SAFETY: as above.
        unsafe { imp::buffer_caps(self.raw.as_ptr()) }
    }

    /// The YCbCr colour encodings this renderer can convert from.
    ///
    /// A *set*, unlike [`TextureOptions::color_encoding`], which names one —
    /// wlroots uses the same C enum in both roles, which is why this crate has
    /// two types for it.
    pub fn color_encodings(&self) -> ColorEncodings {
        // SAFETY: as above.
        unsafe { imp::color_encodings(self.raw.as_ptr()) }
    }

    /// The formats this renderer can sample from, for buffers with `caps`.
    ///
    /// `None` when it can sample from no buffer of that kind at all — the
    /// pixman renderer answers `None` for everything except
    /// [`BufferCaps::DATA_PTR`].
    pub fn texture_formats(&self, caps: BufferCaps) -> Option<DrmFormatSetRef<'_>> {
        // SAFETY: as above; the returned set is the renderer's and cannot
        // outlive this borrow.
        unsafe { imp::texture_formats(self.raw.as_ptr(), caps) }
    }

    /// The DRM device this renderer draws with, borrowed.
    ///
    /// Never an `OwnedFd`: wlroots documents the descriptor as the renderer's,
    /// "it must not close it". `None` for a renderer with no DRM device, which
    /// includes every pixman renderer.
    pub fn drm_fd(&self) -> Option<BorrowedFd<'_>> {
        // SAFETY: as above.
        unsafe { imp::drm_fd(self.raw.as_ptr()) }
    }

    /// Register `wl_shm`, `linux-dmabuf` and the other buffer-factory globals
    /// on `display`.
    ///
    /// Call once. wlroots does not promise idempotence, and the globals belong
    /// to the display from then on.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if a global could not be created.
    pub fn init_wl_display(&self, display: &Display) -> Result<()> {
        // SAFETY: this value owns a live renderer, and the borrow keeps the
        // display alive for the call.
        unsafe { imp::init_wl_display(self.raw.as_ptr(), display.as_ptr()) }
    }

    /// Register only the `wl_shm` global on `display`.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if the global could not be created.
    pub fn init_wl_shm(&self, display: &Display) -> Result<()> {
        // SAFETY: as above.
        unsafe { imp::init_wl_shm(self.raw.as_ptr(), display.as_ptr()) }
    }

    /// Whether the GPU was lost — reset, removed, or otherwise invalidated.
    ///
    /// Latches once set. There is no recovery API in wlroots: every texture,
    /// swapchain and in-flight pass derived from this renderer is invalid, and
    /// the only correct response is to tear the renderer down and build a new
    /// one. A compositor driven by [`Backend::run_all`](crate::Backend::run_all)
    /// hears about the runtime's own renderer through
    /// [`OutputHandler::renderer_lost`](crate::OutputHandler::renderer_lost)
    /// instead of polling this.
    pub fn is_lost(&self) -> bool {
        self.lost.get()
    }

    /// A texture holding a copy of `data`.
    ///
    /// `stride` is in bytes. The pixels are copied twice — once into this
    /// crate's own allocation and once by the renderer — because at least one
    /// renderer keeps reading through the pointer it was handed; see this
    /// module's `texture.rs` doc.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if `data` is shorter than `stride * height`, if
    /// `stride` is shorter than one row of `width` pixels in `format`, or if
    /// any of `stride`, `width`, `height` is zero — wlroots asserts the zeroes
    /// and checks neither of the other two. [`Error::Create`] if the renderer
    /// refused the format.
    pub fn texture_from_pixels(
        &self,
        format: FourCc,
        stride: u32,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<Texture<'_>> {
        if stride == 0 || width == 0 || height == 0 {
            return Err(Error::Operation("Renderer::texture_from_pixels"));
        }
        // A stride shorter than one row is the check that is easy to miss and
        // impossible to notice: the renderer lays an image `width` pixels wide
        // over this pointer and reads a full row from every one of `height`
        // stride-spaced offsets, so `stride * height` is a sufficient bound
        // only once the stride is itself a row's worth of bytes. wlroots checks
        // neither. See `pixel_format.rs`.
        if u64::from(stride) < pixel_format::min_stride_bound(format, u64::from(width)) {
            return Err(Error::Operation("Renderer::texture_from_pixels"));
        }
        // In `u64` throughout so a 32-bit `usize` cannot overflow before the
        // comparison. wlroots reads `stride * height` bytes and never learns
        // how many there are.
        let needed = u64::from(stride) * u64::from(height);
        if (data.len() as u64) < needed {
            return Err(Error::Operation("Renderer::texture_from_pixels"));
        }

        let pixels: Box<[u8]> = data.to_vec().into_boxed_slice();
        // SAFETY: this value owns a live renderer; `pixels` is a live
        // allocation of at least `stride * height` bytes (checked above) whose
        // address the `Texture` below keeps stable for as long as the texture
        // exists — which is what the pixman renderer requires and what a bare
        // `&[u8]` could not promise.
        let raw = unsafe {
            sys::wlr_texture_from_pixels(
                self.raw.as_ptr(),
                format.0,
                stride,
                width,
                height,
                pixels.as_ptr().cast(),
            )
        };
        if raw.is_null() {
            return Err(Error::Create("wlr_texture_from_pixels"));
        }
        // SAFETY: non-null, freshly created by this renderer, and handed the
        // allocation it was built over.
        Ok(unsafe { Texture::from_raw(raw, Some(pixels)) })
    }

    /// A texture importing `attrs`' DMA-BUF.
    ///
    /// The descriptors stay the caller's: wlroots dups whatever it needs to
    /// keep (`types/buffer/dmabuf.c`), so `attrs` still has to be dropped —
    /// which closes them — when the caller is done with it. The returned
    /// texture is **immutable**;
    /// [`Texture::update_from_buffer`] will always refuse it.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if the renderer could not import the buffer.
    pub fn texture_from_dmabuf(&self, attrs: &DmabufAttributes) -> Result<Texture<'_>> {
        // SAFETY: this value owns a live renderer, and `attrs` is a live
        // attributes struct for the call. The parameter is non-`const` in C but
        // wlroots only copies out of it (`wlr_texture_from_dmabuf` →
        // `dmabuf_buffer_create`), so the shared borrow is honest.
        let raw =
            unsafe { sys::wlr_texture_from_dmabuf(self.raw.as_ptr(), attrs.as_ptr().cast_mut()) };
        if raw.is_null() {
            return Err(Error::Create("wlr_texture_from_dmabuf"));
        }
        // SAFETY: non-null and freshly created by this renderer.
        Ok(unsafe { Texture::from_raw(raw, None) })
    }

    /// A texture sampling `buffer`.
    ///
    /// The texture takes its own lock on the buffer, so the buffer outlives it
    /// whatever the caller does with their own reference.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if a mapping opened by
    /// [`Buffer::begin_data_ptr_access`](crate::Buffer::begin_data_ptr_access)
    /// is still live on `buffer`: the shared-memory paths of both renderers
    /// open wlroots' own data-pointer bracket on it
    /// (`pixman_texture_from_buffer`, `gles2_texture_from_buffer`), whose
    /// entry `assert(!buffer->accessing_data_ptr)` would abort the process on
    /// the non-`NDEBUG` builds this crate targets. [`Error::Create`] if the
    /// renderer could not use the buffer.
    pub fn texture_from_buffer(&self, buffer: &Buffer<'_>) -> Result<Texture<'_>> {
        if buffer.data_ptr_access_open() {
            return Err(Error::Operation("wlr_texture_from_buffer"));
        }
        // SAFETY: this value owns a live renderer, and the borrow keeps the
        // buffer alive for the call (after which wlroots' own lock does).
        let raw = unsafe { sys::wlr_texture_from_buffer(self.raw.as_ptr(), buffer.as_ptr()) };
        if raw.is_null() {
            return Err(Error::Create("wlr_texture_from_buffer"));
        }
        // SAFETY: non-null and freshly created by this renderer.
        Ok(unsafe { Texture::from_raw(raw, None) })
    }

    /// A GPU timer for a pass.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if this renderer has no timer support — the pixman
    /// renderer has none.
    pub fn create_timer(&self) -> Result<RenderTimer<'_>> {
        // SAFETY: this value owns a live renderer.
        let raw = unsafe { sys::wlr_render_timer_create(self.raw.as_ptr()) };
        if raw.is_null() {
            return Err(Error::Create("wlr_render_timer_create"));
        }
        // SAFETY: non-null and freshly created by this renderer, which `'_`
        // makes outlive the timer.
        Ok(unsafe { RenderTimer::from_raw(raw) })
    }

    /// Begin a pass drawing into `buffer`.
    ///
    /// The returned pass borrows this renderer, the buffer, and whatever
    /// `options` names — a [`RenderTimer`] most of all, which the pass keeps a
    /// pointer to and writes through when it is submitted (`render/gles2/
    /// pass.c` reads `pass->timer` from inside `submit`). Sharing `'b` between
    /// the buffer and the options is what makes a timer dropped before the
    /// pass a compile error rather than a use-after-free.
    ///
    /// Dropping the pass submits it; see [`RenderPass`].
    ///
    /// # Errors
    ///
    /// [`Error::Reentrant`] if a pass begun on this renderer is still live —
    /// nesting is a stack discipline this crate does not model, and getting it
    /// wrong on the GLES2 renderer corrupts the EGL context rather than
    /// failing. [`Error::Operation`] if `options` name a colour transform or a
    /// signal timeline this renderer does not support — wlroots ignores both
    /// silently, and a colour-managed compositor that is quietly not
    /// colour-managed is worse than one that fails. [`Error::Operation`] also
    /// if a mapping opened by
    /// [`Buffer::begin_data_ptr_access`](crate::Buffer::begin_data_ptr_access)
    /// is still live on `buffer` — the pixman renderer maps the pass's target
    /// through wlroots' own data-pointer bracket, whose entry
    /// `assert(!buffer->accessing_data_ptr)` would abort the process.
    /// [`Error::Create`] if the renderer refused the buffer.
    pub fn begin_buffer_pass<'r, 'b>(
        &'r self,
        buffer: &'b Buffer<'b>,
        options: &BufferPassOptions<'b>,
    ) -> Result<RenderPass<'r, 'b>> {
        if self.pass_live.get() {
            return Err(Error::Reentrant("Renderer::begin_buffer_pass"));
        }
        if buffer.data_ptr_access_open() {
            return Err(Error::Operation("wlr_renderer_begin_buffer_pass"));
        }
        if options.unsupported_by(&self.features()) {
            return Err(Error::Operation("wlr_renderer_begin_buffer_pass"));
        }

        let raw_options = options.to_sys();
        // SAFETY: this value owns a live renderer, the borrow keeps the buffer
        // alive for the pass's life, and `raw_options` outlives the call
        // (wlroots copies what it needs out of it).
        let raw = unsafe {
            sys::wlr_renderer_begin_buffer_pass(
                self.raw.as_ptr(),
                buffer.as_ptr(),
                &raw const raw_options,
            )
        };
        if raw.is_null() {
            return Err(Error::Create("wlr_renderer_begin_buffer_pass"));
        }
        self.pass_live.set(true);
        // SAFETY: non-null, begun on this renderer, and the live-pass flag is
        // set — which is what the pass's own `Drop` clears.
        Ok(unsafe { RenderPass::from_raw(raw, self) })
    }

    /// Release the single-live-pass claim. Called by [`RenderPass`] when it is
    /// submitted, which is the only way a pass ends.
    pub(crate) fn clear_pass_live(&self) {
        self.pass_live.set(false);
    }

    /// This renderer's pixman-specific surface, or `None` if it is not the
    /// pixman renderer.
    ///
    /// Named `as_pixman` because [`Renderer::pixman`] is the constructor. The
    /// three `as_*` accessors are the only way to reach a backend-specific
    /// wlroots call, and the view they hand back *is* the type test — see
    /// `pixman.rs` for why that matters.
    pub fn as_pixman(&self) -> Option<Pixman<'_>> {
        // SAFETY: this value owns a live renderer.
        if !unsafe { sys::wlr_renderer_is_pixman(self.raw.as_ptr()) } {
            return None;
        }
        // SAFETY: live, and the test above is the precondition `Pixman` needs.
        Some(unsafe { Pixman::from_raw(self.raw.as_ptr()) })
    }

    /// This renderer's GLES2-specific surface, or `None` if it is not the GLES2
    /// renderer.
    #[cfg(wlr_has_gles2_renderer)]
    pub fn as_gles2(&self) -> Option<Gles2<'_>> {
        // SAFETY: this value owns a live renderer.
        if !unsafe { sys::wlr_renderer_is_gles2(self.raw.as_ptr()) } {
            return None;
        }
        // SAFETY: live, and the test above is the precondition `Gles2` needs.
        Some(unsafe { Gles2::from_raw(self.raw.as_ptr()) })
    }

    /// This renderer's Vulkan-specific surface, or `None` if it is not the
    /// Vulkan renderer.
    #[cfg(wlr_has_vulkan_renderer)]
    pub fn as_vulkan(&self) -> Option<Vk<'_>> {
        // SAFETY: this value owns a live renderer.
        if !unsafe { sys::wlr_renderer_is_vk(self.raw.as_ptr()) } {
            return None;
        }
        // SAFETY: live, and the test above is the precondition `Vk` needs.
        Some(unsafe { Vk::from_raw(self.raw.as_ptr()) })
    }

    /// Create a GLES2 renderer on the DRM device `fd` names.
    ///
    /// `fd` is **borrowed**: wlroots uses it only to identify the device and
    /// opens its own render node (`open_render_node` in `render/egl.c`), so it
    /// neither dups nor closes it and the caller may close it as soon as this
    /// returns.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if EGL initialisation failed — no GBM device, no
    /// suitable EGL config, missing extensions.
    #[cfg(wlr_has_gles2_renderer)]
    pub fn gles2_with_drm_fd(fd: BorrowedFd<'_>) -> Result<Renderer> {
        // SAFETY: the borrow keeps the descriptor open for the call, which is
        // the only time wlroots reads it.
        let raw = unsafe { sys::wlr_gles2_renderer_create_with_drm_fd(fd.as_raw_fd()) };
        Renderer::adopt(raw, "wlr_gles2_renderer_create_with_drm_fd")
    }

    /// Create a GLES2 renderer on a caller-initialised EGL display and context.
    ///
    /// **Consumes `egl`**, which is the only way to release one: wlroots takes
    /// ownership and destroys it with the renderer, and there is no
    /// `wlr_egl_destroy`. Taking it by value is what makes that a move rather
    /// than a rule — see `egl.rs`.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if the renderer could not be built. `egl` is consumed
    /// either way: wlroots' failure paths do not hand it back, and there is no
    /// destructor to give the caller.
    #[cfg(wlr_has_gles2_renderer)]
    pub fn gles2_from_egl(egl: Egl) -> Result<Renderer> {
        // SAFETY: `egl` is a live `wlr_egl` this call takes ownership of; the
        // `into_raw` consumes the wrapper so nothing here can use it again.
        let raw = unsafe { sys::wlr_gles2_renderer_create(egl.into_raw()) };
        Renderer::adopt(raw, "wlr_gles2_renderer_create")
    }

    /// Create a Vulkan renderer on the DRM device `fd` names.
    ///
    /// `fd` is **borrowed**: wlroots uses it only to match a physical device
    /// (`render/vulkan/renderer.c`) and neither dups nor closes it.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if no Vulkan device matched, or initialisation failed.
    #[cfg(wlr_has_vulkan_renderer)]
    pub fn vulkan_with_drm_fd(fd: BorrowedFd<'_>) -> Result<Renderer> {
        // SAFETY: the borrow keeps the descriptor open for the call, which is
        // the only time wlroots reads it.
        let raw = unsafe { sys::wlr_vk_renderer_create_with_drm_fd(fd.as_raw_fd()) };
        Renderer::adopt(raw, "wlr_vk_renderer_create_with_drm_fd")
    }
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer")
            .field("buffer_caps", &self.buffer_caps())
            .field("features", &self.features())
            .field("lost", &self.is_lost())
            .finish()
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // Unlink first, destroy second, and the order is not stylistic:
        // `wlr_renderer_destroy` emits `events.destroy` and then **asserts**
        // that both `events.destroy` and `events.lost` have no listeners left
        // (`render/wlr_renderer.c`). Those assertions are compiled into the
        // distro's `libwlroots-0.20.so`, so a listener still linked here would
        // abort the process rather than leak.
        drop(self.lost_watch.take());
        // SAFETY: this value owns the renderer — nothing in wlroots destroys
        // one it did not create, verified against 0.20.2 — so this is the one
        // and only destroy. Every `Texture`, `RenderTimer`, `Allocator` and
        // `RenderPass` derived from it borrowed it, so none can be alive here.
        unsafe { sys::wlr_renderer_destroy(self.raw.as_ptr()) };
    }
}

/// A renderer this crate does **not** own — the one
/// [`Runtime::init_graphics`](crate::Runtime::init_graphics) created, reached
/// through [`Runtime::renderer_ref`](crate::Runtime::renderer_ref).
///
/// Carries the read-only and texture-creating surface of [`Renderer`] and
/// nothing that could free it. [`Renderer::begin_buffer_pass`] is deliberately
/// absent: the one-live-pass guard is state on the owning `Renderer`, and a
/// view cannot see the passes wlroots' own scene and output code begins on the
/// same renderer.
#[derive(Clone, Copy)]
pub struct RendererRef<'a> {
    raw: NonNull<sys::wlr_renderer>,
    _scope: std::marker::PhantomData<&'a ()>,
}

impl<'a> RendererRef<'a> {
    /// # Safety
    ///
    /// `raw` must point at a live `wlr_renderer` that outlives `'a` and that
    /// this value must not free.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_renderer) -> RendererRef<'a> {
        RendererRef {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_renderer"),
            _scope: std::marker::PhantomData,
        }
    }

    /// The raw renderer, for interop. Borrowed — destroying it through this
    /// pointer would free an object the runtime still uses.
    pub fn as_ptr(&self) -> *mut sys::wlr_renderer {
        self.raw.as_ptr()
    }

    /// What this renderer supports beyond drawing.
    pub fn features(&self) -> RendererFeatures {
        // SAFETY: `'a` guarantees the renderer is live.
        unsafe { imp::features(self.raw.as_ptr()) }
    }

    /// The kinds of buffer this renderer can render into.
    pub fn buffer_caps(&self) -> BufferCaps {
        // SAFETY: as above.
        unsafe { imp::buffer_caps(self.raw.as_ptr()) }
    }

    /// The YCbCr colour encodings this renderer can convert from.
    pub fn color_encodings(&self) -> ColorEncodings {
        // SAFETY: as above.
        unsafe { imp::color_encodings(self.raw.as_ptr()) }
    }

    /// The formats this renderer can sample from, for buffers with `caps`.
    pub fn texture_formats(&self, caps: BufferCaps) -> Option<DrmFormatSetRef<'a>> {
        // SAFETY: as above; the set is the renderer's and lives as long as it.
        unsafe { imp::texture_formats(self.raw.as_ptr(), caps) }
    }

    /// The DRM device this renderer draws with, borrowed. Never closed here.
    pub fn drm_fd(&self) -> Option<BorrowedFd<'a>> {
        // SAFETY: as above.
        unsafe { imp::drm_fd(self.raw.as_ptr()) }
    }

    /// Register `wl_shm`, `linux-dmabuf` and the other buffer-factory globals
    /// on `display`.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if a global could not be created.
    pub fn init_wl_display(&self, display: &Display) -> Result<()> {
        // SAFETY: as above, plus a live display from the caller's borrow.
        unsafe { imp::init_wl_display(self.raw.as_ptr(), display.as_ptr()) }
    }

    /// Register only the `wl_shm` global on `display`.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if the global could not be created.
    pub fn init_wl_shm(&self, display: &Display) -> Result<()> {
        // SAFETY: as above.
        unsafe { imp::init_wl_shm(self.raw.as_ptr(), display.as_ptr()) }
    }
}

impl std::fmt::Debug for RendererRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RendererRef")
            .field("buffer_caps", &self.buffer_caps())
            .field("features", &self.features())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three capability bits are wlroots ABI. Pin them against wlroots'
    /// own enum rather than against the comment next to them.
    #[test]
    fn buffer_cap_bits_match_wlroots() {
        assert_eq!(BufferCaps::DATA_PTR.bits(), 1);
        assert_eq!(BufferCaps::DMABUF.bits(), 2);
        assert_eq!(BufferCaps::SHM.bits(), 4);
        assert_eq!(
            BufferCaps::DATA_PTR.bits(),
            sys::wlr_buffer_cap::WLR_BUFFER_CAP_DATA_PTR.0
        );
        assert_eq!(
            BufferCaps::DMABUF.bits(),
            sys::wlr_buffer_cap::WLR_BUFFER_CAP_DMABUF.0
        );
        assert_eq!(
            BufferCaps::SHM.bits(),
            sys::wlr_buffer_cap::WLR_BUFFER_CAP_SHM.0
        );
    }

    #[test]
    fn buffer_caps_contains_is_a_subset_test_not_an_overlap_test() {
        let both = BufferCaps::DATA_PTR | BufferCaps::SHM;
        assert!(both.contains(BufferCaps::DATA_PTR));
        assert!(both.contains(both));
        assert!(!both.contains(BufferCaps::DMABUF));
        assert!(!both.contains(BufferCaps::DMABUF | BufferCaps::SHM));
        assert!(both.contains(BufferCaps::NONE));
        assert!(BufferCaps::NONE.is_empty());
    }

    /// Every type here is `!Send` and `!Sync`, which is the premise the whole
    /// crate's thread-scoped handler guard rests on (see `dispatch.rs`). A
    /// renderer moved to another thread would let two threads drive one
    /// wlroots renderer, and the GLES2 renderer's global EGL context makes that
    /// immediately unsound.
    ///
    /// Asserted rather than left to inference: every one of these is `!Send`
    /// today only because of a raw pointer or a `Cell` in its body, and a
    /// future field change could silently make one of them `Send`.
    #[test]
    fn nothing_in_the_render_module_crosses_a_thread() {
        use static_assertions::assert_not_impl_any;

        assert_not_impl_any!(Renderer: Send, Sync);
        assert_not_impl_any!(RendererRef<'static>: Send, Sync);
        assert_not_impl_any!(Texture<'static>: Send, Sync);
        assert_not_impl_any!(RenderPass<'static, 'static>: Send, Sync);
        assert_not_impl_any!(RenderTimer<'static>: Send, Sync);
        assert_not_impl_any!(Allocator<'static>: Send, Sync);
        assert_not_impl_any!(AllocatorRef<'static>: Send, Sync);
        assert_not_impl_any!(Swapchain<'static>: Send, Sync);
        assert_not_impl_any!(OwnedBuffer<'static>: Send, Sync);
        assert_not_impl_any!(LockedBuffer<'static>: Send, Sync);
        assert_not_impl_any!(crate::Buffer<'static>: Send, Sync);
        assert_not_impl_any!(DrmFormatSet: Send, Sync);
        assert_not_impl_any!(DrmFormatSetRef<'static>: Send, Sync);
        assert_not_impl_any!(DrmFormatRef<'static>: Send, Sync);
    }

    /// The two exceptions, pinned deliberately rather than left to drift.
    ///
    /// [`DmabufAttributes`] and [`DmabufAttributesRef`] reach no wlroots object
    /// at all: they are four integers, a plane count and a fixed array of file
    /// descriptors. Nothing about moving one to another thread is unsound —
    /// descriptors are process-wide, and the wlroots calls that *consume* the
    /// attributes take a `&Renderer`, which is `!Send`, so the call itself
    /// still cannot leave the compositor thread.
    ///
    /// This is asserted so that adding a field which does reach wlroots (a
    /// cached buffer pointer, say) fails here rather than silently widening
    /// what a consumer may do with them.
    #[test]
    fn the_dmabuf_attribute_types_are_plain_data_and_say_so() {
        use static_assertions::assert_impl_all;

        assert_impl_all!(DmabufAttributes: Send, Sync);
        assert_impl_all!(DmabufAttributesRef<'static>: Send, Sync);
    }

    /// The pixman renderer needs no backend and no GPU, so this much can be
    /// asserted in the `--lib` test binary; `tests/render.rs` takes it further.
    #[test]
    fn a_pixman_renderer_reports_data_ptr_and_no_drm_device() {
        let renderer = Renderer::pixman().expect("pixman renderer");
        assert_eq!(renderer.buffer_caps(), BufferCaps::DATA_PTR);
        assert!(renderer.drm_fd().is_none());
        assert!(!renderer.is_lost());
        assert!(
            renderer.texture_formats(BufferCaps::DATA_PTR).is_some(),
            "pixman samples from mappable buffers"
        );
        assert!(
            renderer.texture_formats(BufferCaps::DMABUF).is_none(),
            "pixman samples from nothing else"
        );
    }

    /// The `lost` watch is linked at creation and must be unlinked before
    /// `wlr_renderer_destroy` runs its "no listeners left" assertion — an
    /// assertion that aborts, so this test failing looks like a crash rather
    /// than a failure.
    #[test]
    fn destroying_a_renderer_does_not_trip_the_empty_listener_list_assertion() {
        for _ in 0..8 {
            let renderer = Renderer::pixman().expect("pixman renderer");
            drop(renderer);
        }
    }
}
