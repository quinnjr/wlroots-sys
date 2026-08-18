//! Textures: pixels a renderer can sample from.
//!
//! # Why a texture borrows its renderer
//!
//! `wlr_renderer_destroy`'s own documentation says textures "must be destroyed
//! separately", and wlroots means it in one direction only: a renderer that
//! outlives its textures is fine, a texture that outlives its renderer is a
//! use-after-free. The pixman renderer makes that concrete — `pixman_destroy`
//! walks its texture list and destroys every texture still on it, so a
//! `wlr_texture_destroy` afterwards is a double free.
//!
//! [`Texture<'r>`] therefore borrows the [`Renderer`](crate::Renderer) it was
//! made from. Dropping the renderer first is a compile error rather than a
//! documented rule (`tests/ui/texture_escape.rs` pins it).
//!
//! # `texture_from_pixels` copies, and has to
//!
//! `wlr_texture_from_pixels` wraps the caller's pointer in a
//! `wlr_readonly_data_buffer` and immediately drops it. If the renderer took a
//! lock (they all do), wlroots copies the pixels into its own allocation and
//! repoints the *buffer* at the copy — but the pixman renderer's
//! `pixman_image` was already built over the **original** pointer, and
//! `texture_read_pixels` reads through that image without re-checking
//! (`render/pixman/renderer.c`, `types/buffer/readonly_data.c`). So the caller's
//! memory has to stay put for the texture's whole life, which a `&[u8]`
//! argument cannot promise. [`Texture`] keeps its own copy of the pixels and
//! hands wlroots a pointer into that instead.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::buffer::Buffer;
use crate::error::{Error, Result};
use crate::geom::Box2D;
use crate::region::Region;
use crate::sys;

use super::format::FourCc;
use super::pixel_format::min_stride_bound;

/// A renderer's texture. Dropping it calls `wlr_texture_destroy`.
///
/// `'r` is the borrow of the [`Renderer`](crate::Renderer) that made it; see
/// this module's own doc for why that borrow is load-bearing rather than
/// decorative.
pub struct Texture<'r> {
    raw: NonNull<sys::wlr_texture>,

    /// The pixels [`Renderer::texture_from_pixels`](crate::Renderer::texture_from_pixels)
    /// copied, kept alive because at least one renderer keeps reading through
    /// the pointer it was handed. `None` for every other constructor. Boxed so
    /// the address survives moving the `Texture`.
    _pixels: Option<Box<[u8]>>,

    _renderer: PhantomData<&'r ()>,
}

impl<'r> Texture<'r> {
    /// # Safety
    ///
    /// * `raw` must be a live texture the caller is handing ownership of.
    /// * `pixels`, if any, must be the allocation `raw` was built over.
    /// * The texture must not outlive the renderer that created it, which `'r`
    ///   is what enforces at every call site.
    pub(crate) unsafe fn from_raw(
        raw: *mut sys::wlr_texture,
        pixels: Option<Box<[u8]>>,
    ) -> Texture<'r> {
        Texture {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_texture"),
            _pixels: pixels,
            _renderer: PhantomData,
        }
    }

    /// The raw texture, for interop with code that speaks `wlr-sys` directly.
    ///
    /// Borrowed, never owned: destroying it through this pointer leaves this
    /// value's `Drop` destroying it again.
    pub fn as_ptr(&self) -> *mut sys::wlr_texture {
        self.raw.as_ptr()
    }

    /// The renderer this texture belongs to, as a raw pointer.
    ///
    /// Exposed only so an in-crate caller can check that a texture and a render
    /// pass belong to the same renderer — mixing them trips a wlroots assertion
    /// rather than failing gracefully.
    pub(crate) fn renderer_ptr(&self) -> *mut sys::wlr_renderer {
        // SAFETY: the handle's lifetime guarantees the texture is live.
        unsafe { (*self.raw.as_ptr()).renderer }
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        // SAFETY: the handle's lifetime guarantees the texture is live.
        unsafe { (*self.raw.as_ptr()).width }
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        // SAFETY: as above.
        unsafe { (*self.raw.as_ptr()).height }
    }

    /// Replace this texture's contents with `buffer`'s.
    ///
    /// `damage` is an optimisation hint naming the part of the buffer that
    /// actually changed; `None` means the whole buffer, which this crate
    /// materialises as a region covering it. That is not decorative:
    /// `wlr_texture_update_from_buffer` dereferences `damage` unconditionally
    /// (`render/wlr_texture.c` reads `pixman_region32_extents(damage)` before
    /// dispatching), so a null would be a straight null dereference.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] — and this is a **routine** outcome, not a fault.
    /// wlroots rejects the update whenever the texture is immutable, the
    /// buffer's type or format is unsupported, its size differs, or the damage
    /// falls outside it; the pixman renderer implements no update path at all
    /// and always refuses. Its own documentation says callers "must be prepared
    /// to fall back to re-creating the texture from scratch via
    /// `wlr_texture_from_buffer()`" — [`Renderer::texture_from_buffer`](crate::Renderer::texture_from_buffer)
    /// here. Treating this as fatal is a bug.
    pub fn update_from_buffer(&self, buffer: &Buffer<'_>, damage: Option<&Region>) -> Result<()> {
        let whole;
        let damage = match damage {
            Some(damage) => damage,
            None => {
                whole = Region::from_box(Box2D::new(0, 0, buffer.width(), buffer.height()));
                &whole
            }
        };
        // SAFETY: the handle's lifetime guarantees the texture is live, and the
        // borrow does the same for the buffer and the region. wlroots reads the
        // region and never retains it past the call.
        let ok = unsafe {
            sys::wlr_texture_update_from_buffer(self.raw.as_ptr(), buffer.as_ptr(), damage.as_ptr())
        };
        if ok {
            Ok(())
        } else {
            Err(Error::Operation("wlr_texture_update_from_buffer"))
        }
    }

    /// Read pixels back out of this texture into `options`' destination.
    ///
    /// The slow path, and not every renderer supports every format; check
    /// [`preferred_read_format`](Texture::preferred_read_format) first.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if the renderer has no read-back path or refused
    /// the requested format, and if the destination cannot hold what the read
    /// would write — wlroots checks neither the destination's length nor the
    /// stride against the width, and gets both wrong by writing past the end.
    pub fn read_pixels(&self, options: &mut ReadPixels<'_>) -> Result<()> {
        let src = options.effective_src_box(self.width(), self.height());
        if !options.fits(&src) {
            return Err(Error::Operation("wlr_texture_read_pixels"));
        }

        let raw = sys::wlr_texture_read_pixels_options {
            data: options.data.as_mut_ptr().cast(),
            format: options.format.0,
            stride: options.stride,
            dst_x: options.dst_x,
            dst_y: options.dst_y,
            // A zeroed (empty) box is how wlroots spells "the whole texture";
            // `effective_src_box` above has already resolved which one this is,
            // so pass what the caller asked for rather than the resolution.
            src_box: sys::wlr_box {
                x: options.src_box.x,
                y: options.src_box.y,
                width: options.src_box.width,
                height: options.src_box.height,
            },
        };
        // SAFETY: the handle's lifetime guarantees the texture is live;
        // `raw.data` points into the caller's slice, which `options` borrows
        // mutably for this call, and the bounds check above proved the slice is
        // long enough for what wlroots will write into it.
        let ok = unsafe { sys::wlr_texture_read_pixels(self.raw.as_ptr(), &raw const raw) };
        if ok {
            Ok(())
        } else {
            Err(Error::Operation("wlr_texture_read_pixels"))
        }
    }

    /// The format this renderer would rather write in
    /// [`read_pixels`](Texture::read_pixels).
    ///
    /// [`FourCc::INVALID`] when the renderer has no preference (wlroots returns
    /// `DRM_FORMAT_INVALID` when the texture implements no read-back at all).
    pub fn preferred_read_format(&self) -> FourCc {
        // SAFETY: the handle's lifetime guarantees the texture is live.
        FourCc(unsafe { sys::wlr_texture_preferred_read_format(self.raw.as_ptr()) })
    }

    /// How this texture is bound in GL, or `None` if it is not a GLES2
    /// texture.
    ///
    /// `wlr_gles2_texture_get_attribs` has no type test of its own and is
    /// undefined behaviour on a texture of another kind, so the test is done
    /// here and reported as the `Option` — the same bargain the backend view
    /// types make for renderers.
    #[cfg(wlr_has_gles2_renderer)]
    pub fn gles2_attribs(&self) -> Option<super::Gles2TextureAttribs> {
        // SAFETY: the handle's lifetime guarantees the texture is live.
        if !unsafe { sys::wlr_texture_is_gles2(self.raw.as_ptr()) } {
            return None;
        }
        let mut attribs = sys::wlr_gles2_texture_attribs {
            target: 0,
            tex: 0,
            has_alpha: false,
        };
        // SAFETY: live texture, proved GLES2 by the test above — which is the
        // precondition — and `attribs` is a live local wlroots fills in.
        unsafe { sys::wlr_gles2_texture_get_attribs(self.raw.as_ptr(), &raw mut attribs) };
        Some(super::Gles2TextureAttribs {
            target: attribs.target,
            tex: attribs.tex,
            has_alpha: attribs.has_alpha,
        })
    }

    /// The `VkImage` backing this texture, or `None` if it is not a Vulkan
    /// texture.
    ///
    /// As with the GLES2 accessor above, `wlr_vk_texture_get_image_attribs`
    /// requires the type test the `Option` reports.
    #[cfg(wlr_has_vulkan_renderer)]
    pub fn vulkan_attribs(&self) -> Option<super::VkImageAttribs> {
        // SAFETY: the handle's lifetime guarantees the texture is live.
        if !unsafe { sys::wlr_texture_is_vk(self.raw.as_ptr()) } {
            return None;
        }
        let mut attribs = sys::wlr_vk_image_attribs {
            image: std::ptr::null_mut(),
            layout: sys::VkImageLayout(0),
            format: sys::VkFormat(0),
        };
        // SAFETY: live texture, proved Vulkan by the test above, and `attribs`
        // is a live local wlroots fills in.
        unsafe { sys::wlr_vk_texture_get_image_attribs(self.raw.as_ptr(), &raw mut attribs) };
        Some(super::VkImageAttribs {
            image: attribs.image,
            layout: attribs.layout.0,
            format: attribs.format.0,
        })
    }

    /// Whether this Vulkan texture's format has an alpha channel, or `None` if
    /// it is not a Vulkan texture.
    ///
    /// The two `None`s are not distinguishable, which is wlroots' own doing:
    /// `wlr_vk_texture_has_alpha` returns a bare `bool` and has no answer for
    /// "not a Vulkan texture" other than undefined behaviour.
    #[cfg(wlr_has_vulkan_renderer)]
    pub fn vulkan_has_alpha(&self) -> Option<bool> {
        // SAFETY: the handle's lifetime guarantees the texture is live.
        if !unsafe { sys::wlr_texture_is_vk(self.raw.as_ptr()) } {
            return None;
        }
        // SAFETY: live texture, proved Vulkan by the test above.
        Some(unsafe { sys::wlr_vk_texture_has_alpha(self.raw.as_ptr()) })
    }

    /// The `pixman_image_t` backing this texture, or `None` if it is not a
    /// pixman texture.
    ///
    /// # Safety
    ///
    /// The image is **borrowed**: the texture owns it and destroys it, so it
    /// must never be `pixman_image_unref`ed, and it dies with the texture.
    /// Using it at all requires pixman, which this crate does not wrap — see
    /// `render/pixman.rs`.
    pub unsafe fn pixman_image(&self) -> Option<*mut sys::pixman_image_t> {
        // SAFETY: the handle's lifetime guarantees the texture is live.
        if !unsafe { sys::wlr_texture_is_pixman(self.raw.as_ptr()) } {
            return None;
        }
        // SAFETY: live texture, proved pixman by the test above.
        let image = unsafe { sys::wlr_pixman_texture_get_image(self.raw.as_ptr()) };
        if image.is_null() {
            return None;
        }
        Some(image)
    }
}

impl std::fmt::Debug for Texture<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Texture")
            .field("width", &self.width())
            .field("height", &self.height())
            .finish()
    }
}

impl Drop for Texture<'_> {
    fn drop(&mut self) {
        // SAFETY: this value owns the texture (every constructor takes
        // ownership), and `'r` guarantees the renderer that has to outlive it
        // still does. Nothing else destroys it: wlroots only destroys textures
        // from its own renderer teardown, which `'r` orders after this.
        unsafe { sys::wlr_texture_destroy(self.raw.as_ptr()) };
    }
}

/// Where and how [`Texture::read_pixels`] writes the pixels it reads.
///
/// Mirrors `wlr_texture_read_pixels_options`, with the destination as a
/// borrowed slice rather than a raw pointer and a length, so the bounds check
/// wlroots does not do has something to check against.
pub struct ReadPixels<'a> {
    data: &'a mut [u8],
    format: FourCc,
    stride: u32,
    dst_x: u32,
    dst_y: u32,
    /// Zeroed (empty) means "the whole texture", which is wlroots' own
    /// convention for this field.
    src_box: Box2D,
}

impl<'a> ReadPixels<'a> {
    /// Read into `data`, writing `format` at `stride` bytes per row.
    pub fn new(data: &'a mut [u8], format: FourCc, stride: u32) -> ReadPixels<'a> {
        ReadPixels {
            data,
            format,
            stride,
            dst_x: 0,
            dst_y: 0,
            src_box: Box2D::default(),
        }
    }

    /// Start writing at `(x, y)` within the destination instead of its origin.
    #[must_use]
    pub fn dst(mut self, x: u32, y: u32) -> Self {
        self.dst_x = x;
        self.dst_y = y;
        self
    }

    /// Read only `b` out of the texture instead of all of it.
    #[must_use]
    pub fn src_box(mut self, b: Box2D) -> Self {
        self.src_box = b;
        self
    }

    /// What wlroots will actually read, given the texture's size: the caller's
    /// box, or the whole texture when they left it empty.
    fn effective_src_box(&self, width: u32, height: u32) -> Box2D {
        if self.src_box.empty() {
            Box2D::new(0, 0, width as i32, height as i32)
        } else {
            self.src_box
        }
    }

    /// Whether the destination slice can hold what a read of `src` would write.
    ///
    /// wlroots computes the first byte it writes as
    /// `data + min_stride(format, dst_x) + dst_y * stride`
    /// (`wlr_texture_read_pixel_options_get_data`) and then writes `src.height`
    /// rows `stride` bytes apart, **each `min_stride(format, src.width)` bytes
    /// long**, through a pixman image (or a `glReadPixels` row walk) built over
    /// that pointer. It never learns how long the destination is, which is what
    /// this has to make up for.
    ///
    /// Charging the last row its full width is the part that is easy to get
    /// wrong and impossible to notice: `rows * stride` alone is a correct bound
    /// only while `stride` is at least a row's worth of bytes, and wlroots
    /// never checks that it is. A `stride` of 4 with an 8-pixel-wide
    /// `ARGB8888` read writes 28 bytes past a slice that `rows * stride` says
    /// is exactly big enough. So the row length is counted from the format
    /// rather than from the stride — see `pixel_format.rs`.
    ///
    /// Every step is in `u64` so a 32-bit `usize` cannot overflow before the
    /// comparison.
    fn fits(&self, src: &Box2D) -> bool {
        if src.width <= 0 || src.height <= 0 {
            return false;
        }
        let stride = u64::from(self.stride);
        // `src.height - 1` rows are skipped over; the last one is charged its
        // real length below rather than a stride.
        let skipped = (src.height as u64 - 1).saturating_add(u64::from(self.dst_y));
        let needed = min_stride_bound(self.format, u64::from(self.dst_x))
            .saturating_add(skipped.saturating_mul(stride))
            .saturating_add(min_stride_bound(self.format, src.width as u64));
        needed <= self.data.len() as u64
    }
}

impl std::fmt::Debug for ReadPixels<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadPixels")
            .field("len", &self.data.len())
            .field("format", &self.format)
            .field("stride", &self.stride)
            .field("dst", &(self.dst_x, self.dst_y))
            .field("src_box", &self.src_box)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty source box means "the whole texture", exactly as wlroots reads
    /// a zeroed `wlr_box`.
    #[test]
    fn an_empty_source_box_resolves_to_the_whole_texture() {
        let mut data = vec![0u8; 64];
        let opts = ReadPixels::new(&mut data, FourCc::ARGB8888, 16);
        assert_eq!(opts.effective_src_box(4, 4), Box2D::new(0, 0, 4, 4));
    }

    #[test]
    fn an_explicit_source_box_is_used_verbatim() {
        let mut data = vec![0u8; 64];
        let opts = ReadPixels::new(&mut data, FourCc::ARGB8888, 16).src_box(Box2D::new(1, 1, 2, 2));
        assert_eq!(opts.effective_src_box(4, 4), Box2D::new(1, 1, 2, 2));
    }

    /// The bounds check is the whole reason this type exists: wlroots writes
    /// `stride * height` bytes into a pointer it never sizes.
    #[test]
    fn a_destination_shorter_than_the_read_is_rejected() {
        let mut data = vec![0u8; 4 * 4 * 4 - 1];
        let opts = ReadPixels::new(&mut data, FourCc::ARGB8888, 16);
        assert!(!opts.fits(&Box2D::new(0, 0, 4, 4)));

        let mut data = vec![0u8; 4 * 4 * 4];
        let opts = ReadPixels::new(&mut data, FourCc::ARGB8888, 16);
        assert!(opts.fits(&Box2D::new(0, 0, 4, 4)));
    }

    /// A destination offset shifts every row down, so it has to be counted.
    #[test]
    fn a_destination_offset_counts_against_the_slice() {
        let mut data = vec![0u8; 4 * 16];
        let opts = ReadPixels::new(&mut data, FourCc::ARGB8888, 16).dst(0, 1);
        assert!(!opts.fits(&Box2D::new(0, 0, 4, 4)));
        assert!(opts.fits(&Box2D::new(0, 0, 4, 3)));
    }

    /// The stride is not a bound on the row length — wlroots writes
    /// `min_stride(format, width)` bytes into every row whatever the stride
    /// says, so a stride shorter than a row must be refused rather than
    /// multiplied out. Measured against the real thing before it was: a
    /// `read_pixels` of an 8×8 `ARGB8888` texture at `stride = 4` into the
    /// 32-byte slice `rows * stride` called sufficient wrote 28 bytes past its
    /// end.
    #[test]
    fn a_stride_shorter_than_one_row_is_rejected() {
        let mut data = vec![0u8; 8 * 4];
        let opts = ReadPixels::new(&mut data, FourCc::ARGB8888, 4);
        assert!(!opts.fits(&Box2D::new(0, 0, 8, 8)));

        // The same read at an honest stride, in a slice sized for it.
        let mut data = vec![0u8; 8 * 8 * 4];
        let opts = ReadPixels::new(&mut data, FourCc::ARGB8888, 8 * 4);
        assert!(opts.fits(&Box2D::new(0, 0, 8, 8)));
    }

    /// The destination x-offset is charged in the destination's own format, not
    /// at a flat rate, so an offset that fits is not refused.
    #[test]
    fn a_destination_x_offset_is_charged_in_the_destination_format() {
        let mut data = vec![0u8; 4 * 4 * 4 + 4];
        let opts = ReadPixels::new(&mut data, FourCc::ARGB8888, 4 * 4).dst(1, 0);
        assert!(opts.fits(&Box2D::new(0, 0, 4, 4)));

        let mut data = vec![0u8; 4 * 4 * 4 + 3];
        let opts = ReadPixels::new(&mut data, FourCc::ARGB8888, 4 * 4).dst(1, 0);
        assert!(!opts.fits(&Box2D::new(0, 0, 4, 4)));
    }

    #[test]
    fn an_empty_read_is_rejected_rather_than_passed_through() {
        let mut data = vec![0u8; 64];
        let opts = ReadPixels::new(&mut data, FourCc::ARGB8888, 16);
        assert!(!opts.fits(&Box2D::new(0, 0, 0, 4)));
        assert!(!opts.fits(&Box2D::new(0, 0, 4, -1)));
    }
}
