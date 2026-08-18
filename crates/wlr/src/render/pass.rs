//! Render passes: the only way to draw into a buffer.
//!
//! # Dropping a pass submits it
//!
//! `wlr_renderer_begin_buffer_pass` allocates, and `wlr_render_pass_submit` is
//! the **only** thing that frees the result — wlroots has no cancel, no abort
//! and no destructor. A pass that is simply forgotten leaks its heap allocation
//! and whatever GPU state the renderer attached to it.
//!
//! So [`RenderPass`]'s `Drop` submits and discards the answer, and
//! [`RenderPass::submit`] is the ordinary path that consumes the pass and hands
//! the answer back. There is no way to spell "throw this pass away without
//! drawing it", because wlroots has none to offer.
//!
//! # One pass at a time
//!
//! The GLES2 renderer keeps the current EGL context in global state and
//! saves/restores it around each operation, treating "a render pass" as one
//! operation (`wlr/render/gles2.h`). Nesting is only legal in a strict stack
//! discipline — the parent pass must not be touched until the nested one is
//! submitted — and this crate does not model it. A second
//! [`Renderer::begin_buffer_pass`](crate::Renderer::begin_buffer_pass) while one
//! pass is still live returns
//! [`Error::Reentrant`] instead.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::error::{Error, Result};
use crate::geom::{Box2D, FBox, Transform};
use crate::region::Region;
use crate::sys;

use super::Renderer;
use super::texture::Texture;

/// A colour in a render pass, each channel in `0..=1`.
///
/// **R, G and B must be pre-multiplied by A**, which is wlroots' own
/// requirement and the one thing that is silently wrong-looking rather than
/// loud when it is not done.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RenderColor {
    /// Red, pre-multiplied by alpha.
    pub r: f32,
    /// Green, pre-multiplied by alpha.
    pub g: f32,
    /// Blue, pre-multiplied by alpha.
    pub b: f32,
    /// Alpha.
    pub a: f32,
}

impl RenderColor {
    /// A colour from its four channels.
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> RenderColor {
        RenderColor { r, g, b, a }
    }
}

impl From<[f32; 4]> for RenderColor {
    /// `[r, g, b, a]` — the order
    /// [`Runtime::add_rect`](crate::Runtime::add_rect) already takes.
    fn from(c: [f32; 4]) -> RenderColor {
        RenderColor::new(c[0], c[1], c[2], c[3])
    }
}

impl From<RenderColor> for [f32; 4] {
    fn from(c: RenderColor) -> [f32; 4] {
        [c.r, c.g, c.b, c.a]
    }
}

/// How a draw combines with what is already in the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum BlendMode {
    /// Blend, treating the source as pre-multiplied. wlroots' default.
    #[default]
    Premultiplied,
    /// Overwrite the destination, ignoring both alphas.
    None,
}

impl From<BlendMode> for sys::wlr_render_blend_mode {
    fn from(mode: BlendMode) -> sys::wlr_render_blend_mode {
        match mode {
            BlendMode::Premultiplied => {
                sys::wlr_render_blend_mode::WLR_RENDER_BLEND_MODE_PREMULTIPLIED
            }
            BlendMode::None => sys::wlr_render_blend_mode::WLR_RENDER_BLEND_MODE_NONE,
        }
    }
}

/// How a texture is sampled when it is scaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum FilterMode {
    /// Interpolate. wlroots' default.
    #[default]
    Bilinear,
    /// Take the nearest texel — what pixel art wants.
    Nearest,
}

impl From<FilterMode> for sys::wlr_scale_filter_mode {
    fn from(mode: FilterMode) -> sys::wlr_scale_filter_mode {
        match mode {
            FilterMode::Bilinear => sys::wlr_scale_filter_mode::WLR_SCALE_FILTER_BILINEAR,
            FilterMode::Nearest => sys::wlr_scale_filter_mode::WLR_SCALE_FILTER_NEAREST,
        }
    }
}

/// What a pass is begun with.
///
/// Every field is optional and the default is what wlroots reads out of a
/// zeroed `wlr_buffer_pass_options`.
///
/// Two of wlroots' four fields have no setter yet: `color_transform` needs the
/// colour wrappers and `signal_timeline` needs the DRM sync-object wrappers,
/// both of which land with the rest of the render module. They are passed as
/// null here, which is what wlroots documents as "no transform" and "no
/// timeline".
#[derive(Debug, Default)]
pub struct BufferPassOptions<'a> {
    timer: Option<&'a RenderTimer<'a>>,
}

impl<'a> BufferPassOptions<'a> {
    /// The default options: no timer, no colour transform, no timeline.
    pub fn new() -> BufferPassOptions<'a> {
        BufferPassOptions { timer: None }
    }

    /// Time this pass with `timer`.
    ///
    /// The borrow is what makes the timer provably outlive the pass, which
    /// wlroots requires and does not check.
    #[must_use]
    pub fn timer(mut self, timer: &'a RenderTimer<'a>) -> Self {
        self.timer = Some(timer);
        self
    }

    pub(crate) fn to_sys(&self) -> sys::wlr_buffer_pass_options {
        sys::wlr_buffer_pass_options {
            timer: self
                .timer
                .map_or(std::ptr::null_mut(), |timer| timer.as_ptr()),
            color_transform: std::ptr::null_mut(),
            signal_timeline: std::ptr::null_mut(),
            signal_point: 0,
        }
    }
}

/// A renderer's GPU timer, for measuring how long a pass took.
///
/// Created by [`Renderer::create_timer`](crate::Renderer::create_timer) and tied
/// to it by `'r`, because `wlr_render_timer_destroy` dispatches through the
/// renderer's own vtable.
pub struct RenderTimer<'r> {
    raw: NonNull<sys::wlr_render_timer>,
    _renderer: PhantomData<&'r ()>,
}

impl<'r> RenderTimer<'r> {
    /// # Safety
    ///
    /// `raw` must be a live timer this value takes ownership of, created by the
    /// renderer `'r` borrows.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_render_timer) -> RenderTimer<'r> {
        RenderTimer {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_render_timer"),
            _renderer: PhantomData,
        }
    }

    pub(crate) fn as_ptr(&self) -> *mut sys::wlr_render_timer {
        self.raw.as_ptr()
    }

    /// How long the timed pass took, in nanoseconds.
    ///
    /// `None` until the pass that used this timer has been submitted *and* the
    /// GPU has finished it — wlroots reports both "not yet" and "not supported"
    /// as `-1`, and gives nothing to tell them apart.
    pub fn duration_ns(&self) -> Option<u64> {
        // SAFETY: this value owns a live timer for as long as it exists.
        let ns = unsafe { sys::wlr_render_timer_get_duration_ns(self.raw.as_ptr()) };
        if ns < 0 { None } else { Some(ns as u64) }
    }
}

impl std::fmt::Debug for RenderTimer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderTimer")
            .field("duration_ns", &self.duration_ns())
            .finish()
    }
}

impl Drop for RenderTimer<'_> {
    fn drop(&mut self) {
        // SAFETY: this value owns the timer, and `'r` keeps the renderer whose
        // vtable frees it alive. A timer a pass still names would be a
        // use-after-free — which is why `BufferPassOptions` borrows it, making
        // the pass outlive-check the compiler's job.
        unsafe { sys::wlr_render_timer_destroy(self.raw.as_ptr()) };
    }
}

/// One textured draw.
///
/// `alpha` is stored in the builder rather than passed as a pointer to a
/// temporary: wlroots takes it as `const float *`, and `&value as *const f32`
/// on a temporary dangles by the time the call reads it — a bug that usually
/// appears to work.
#[derive(Debug)]
pub struct TextureOptions<'a> {
    texture: &'a Texture<'a>,
    src_box: FBox,
    dst_box: Box2D,
    alpha: Option<f32>,
    clip: Option<&'a Region>,
    transform: Transform,
    filter_mode: FilterMode,
    blend_mode: BlendMode,
}

impl<'a> TextureOptions<'a> {
    /// Draw all of `texture`, at its own size, into the pass's origin.
    pub fn new(texture: &'a Texture<'a>) -> TextureOptions<'a> {
        TextureOptions {
            texture,
            // Empty means "the whole texture" / "the texture's size", which is
            // what wlroots reads out of the zeroed struct.
            src_box: FBox::default(),
            dst_box: Box2D::default(),
            alpha: None,
            clip: None,
            transform: Transform::Normal,
            filter_mode: FilterMode::default(),
            blend_mode: BlendMode::default(),
        }
    }

    /// Sample only this part of the texture. Must lie within it.
    #[must_use]
    pub fn src_box(mut self, b: FBox) -> Self {
        self.src_box = b;
        self
    }

    /// Draw into this part of the destination. An empty box means the
    /// texture's own size at the origin.
    #[must_use]
    pub fn dst_box(mut self, b: Box2D) -> Self {
        self.dst_box = b;
        self
    }

    /// Multiply the texture by this alpha. Unset means fully opaque.
    #[must_use]
    pub fn alpha(mut self, alpha: f32) -> Self {
        self.alpha = Some(alpha);
        self
    }

    /// Draw only inside `clip`.
    #[must_use]
    pub fn clip(mut self, clip: &'a Region) -> Self {
        self.clip = Some(clip);
        self
    }

    /// Rotate/flip the texture as it is drawn.
    #[must_use]
    pub fn transform(mut self, transform: Transform) -> Self {
        self.transform = transform;
        self
    }

    /// How to sample when scaling.
    #[must_use]
    pub fn filter(mut self, filter: FilterMode) -> Self {
        self.filter_mode = filter;
        self
    }

    /// How to combine with what is already there.
    #[must_use]
    pub fn blend(mut self, blend: BlendMode) -> Self {
        self.blend_mode = blend;
        self
    }

    /// Whether the source box lies inside the texture.
    ///
    /// `wlr_render_pass_add_texture` **asserts** this, and the assertion is
    /// compiled into the shipped `libwlroots-0.20.so`, so it has to be checked
    /// before the call rather than reported by it.
    fn src_box_in_bounds(&self) -> bool {
        if self.src_box.empty() {
            return true;
        }
        let width = f64::from(self.texture.width());
        let height = f64::from(self.texture.height());
        self.src_box.x >= 0.0
            && self.src_box.y >= 0.0
            && self.src_box.x + self.src_box.width <= width
            && self.src_box.y + self.src_box.height <= height
    }
}

/// One solid-colour rectangle.
#[derive(Debug)]
pub struct RectOptions<'a> {
    box_: Box2D,
    color: RenderColor,
    clip: Option<&'a Region>,
    blend_mode: BlendMode,
}

impl<'a> RectOptions<'a> {
    /// Fill `box_` with `color`. An empty box means the whole destination
    /// buffer, which is how wlroots reads a zeroed one.
    pub fn new(box_: Box2D, color: RenderColor) -> RectOptions<'a> {
        RectOptions {
            box_,
            color,
            clip: None,
            blend_mode: BlendMode::default(),
        }
    }

    /// Draw only inside `clip`.
    #[must_use]
    pub fn clip(mut self, clip: &'a Region) -> Self {
        self.clip = Some(clip);
        self
    }

    /// How to combine with what is already there.
    ///
    /// Note that the pixman renderer overrides this for a fully opaque colour,
    /// which it draws with a plain copy either way.
    #[must_use]
    pub fn blend(mut self, blend: BlendMode) -> Self {
        self.blend_mode = blend;
        self
    }
}

/// A pass recording draws into one buffer.
///
/// Submitted by [`submit`](RenderPass::submit), or by `Drop` if it is not — see
/// this module's own doc for why there is no third option.
pub struct RenderPass<'r, 'b> {
    raw: NonNull<sys::wlr_render_pass>,
    /// Cleared once the pass has been handed to `wlr_render_pass_submit`, which
    /// frees it; `Drop` reads this to avoid submitting a second time.
    submitted: bool,
    renderer: &'r Renderer,
    _buffer: PhantomData<&'b ()>,
}

impl<'r, 'b> RenderPass<'r, 'b> {
    /// # Safety
    ///
    /// * `raw` must be a live pass this value takes ownership of, begun on
    ///   `renderer`.
    /// * `renderer.mark_pass_live()` must already have been called, so this
    ///   value's `Drop` has a live-pass flag to clear.
    pub(crate) unsafe fn from_raw(
        raw: *mut sys::wlr_render_pass,
        renderer: &'r Renderer,
    ) -> RenderPass<'r, 'b> {
        RenderPass {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_render_pass"),
            submitted: false,
            renderer,
            _buffer: PhantomData,
        }
    }

    /// Record a textured draw.
    ///
    /// # Errors
    ///
    /// * [`Error::Mismatch`] if the texture belongs to a different renderer.
    ///   Every renderer's `add_texture` asserts on the texture's identity
    ///   before touching it, and the assertion aborts rather than returning.
    /// * [`Error::Operation`] if the source box falls outside the texture,
    ///   which wlroots also asserts.
    pub fn add_texture(&mut self, options: &TextureOptions<'_>) -> Result<()> {
        if options.texture.renderer_ptr() != self.renderer.as_ptr() {
            return Err(Error::Mismatch("RenderPass::add_texture"));
        }
        if !options.src_box_in_bounds() {
            return Err(Error::Operation("wlr_render_pass_add_texture"));
        }

        // Held in a local for the duration of the call: wlroots reads through
        // this pointer while `add_texture` runs and never retains it.
        let alpha = options.alpha;
        let raw = sys::wlr_render_texture_options {
            texture: options.texture.as_ptr(),
            src_box: sys::wlr_fbox {
                x: options.src_box.x,
                y: options.src_box.y,
                width: options.src_box.width,
                height: options.src_box.height,
            },
            dst_box: sys::wlr_box {
                x: options.dst_box.x,
                y: options.dst_box.y,
                width: options.dst_box.width,
                height: options.dst_box.height,
            },
            alpha: alpha
                .as_ref()
                .map_or(std::ptr::null(), |alpha| &raw const *alpha),
            clip: options
                .clip
                .map_or(std::ptr::null(), |region| region.as_ptr()),
            transform: options.transform.into(),
            filter_mode: options.filter_mode.into(),
            blend_mode: options.blend_mode.into(),
            // Zero is "unset" for all four of these, not a named enum value —
            // `wlr_color_transfer_function` has no zero variant at all. The
            // colour setters arrive with the colour wrappers.
            transfer_function: sys::wlr_color_transfer_function(0),
            primaries: std::ptr::null(),
            color_encoding: sys::wlr_color_encoding::WLR_COLOR_ENCODING_NONE,
            color_range: sys::wlr_color_range::WLR_COLOR_RANGE_NONE,
            luminance_multiplier: std::ptr::null(),
            wait_timeline: std::ptr::null_mut(),
            wait_point: 0,
        };
        // SAFETY: the pass is live (this value owns it until `submit`), the
        // texture is live and belongs to this renderer (checked above), the
        // clip region and the alpha local outlive the call, and the source box
        // satisfies wlroots' own assertion.
        unsafe { sys::wlr_render_pass_add_texture(self.raw.as_ptr(), &raw const raw) };
        Ok(())
    }

    /// Record a solid-colour rectangle.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] for a negative width or height:
    /// `wlr_render_pass_add_rect` asserts both are non-negative. A zero-sized
    /// box is not an error — wlroots reads it as "the whole buffer".
    pub fn add_rect(&mut self, options: &RectOptions<'_>) -> Result<()> {
        if options.box_.width < 0 || options.box_.height < 0 {
            return Err(Error::Operation("wlr_render_pass_add_rect"));
        }

        let raw = sys::wlr_render_rect_options {
            box_: sys::wlr_box {
                x: options.box_.x,
                y: options.box_.y,
                width: options.box_.width,
                height: options.box_.height,
            },
            color: sys::wlr_render_color {
                r: options.color.r,
                g: options.color.g,
                b: options.color.b,
                a: options.color.a,
            },
            clip: options
                .clip
                .map_or(std::ptr::null(), |region| region.as_ptr()),
            blend_mode: options.blend_mode.into(),
        };
        // SAFETY: the pass is live, the clip region outlives the call, and the
        // box satisfies wlroots' own assertion (checked above).
        unsafe { sys::wlr_render_pass_add_rect(self.raw.as_ptr(), &raw const raw) };
        Ok(())
    }

    /// Hand the recorded draws to the GPU, consuming the pass.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if the submission failed. The pass is freed either
    /// way — there is nothing left to retry with.
    pub fn submit(mut self) -> Result<()> {
        if self.submit_once() {
            Ok(())
        } else {
            Err(Error::Operation("wlr_render_pass_submit"))
        }
    }

    /// Submit unless that has already happened. Returns wlroots' answer, or
    /// `true` for a pass that was already submitted (nothing failed here).
    fn submit_once(&mut self) -> bool {
        if self.submitted {
            return true;
        }
        self.submitted = true;
        self.renderer.clear_pass_live();
        // SAFETY: the pass is live and this is the one call that consumes it;
        // `submitted` above makes a second call unreachable.
        unsafe { sys::wlr_render_pass_submit(self.raw.as_ptr()) }
    }
}

impl std::fmt::Debug for RenderPass<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderPass")
            .field("submitted", &self.submitted)
            .finish()
    }
}

impl Drop for RenderPass<'_, '_> {
    fn drop(&mut self) {
        // Submitting is the only way to free a pass, so a dropped one is
        // submitted and its answer discarded. There is nowhere to report a
        // failed submission from a destructor, and leaking the pass instead
        // would leak GPU memory *and* leave the renderer permanently marked as
        // having a live pass.
        self.submit_once();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three enum mappings are C ABI. Pin them against wlroots' own
    /// constants rather than against the `as` casts a reader cannot check.
    #[test]
    fn blend_and_filter_modes_match_wlroots() {
        assert_eq!(
            sys::wlr_render_blend_mode::from(BlendMode::Premultiplied),
            sys::wlr_render_blend_mode::WLR_RENDER_BLEND_MODE_PREMULTIPLIED
        );
        assert_eq!(
            sys::wlr_render_blend_mode::from(BlendMode::None),
            sys::wlr_render_blend_mode::WLR_RENDER_BLEND_MODE_NONE
        );
        assert_eq!(
            sys::wlr_scale_filter_mode::from(FilterMode::Bilinear),
            sys::wlr_scale_filter_mode::WLR_SCALE_FILTER_BILINEAR
        );
        assert_eq!(
            sys::wlr_scale_filter_mode::from(FilterMode::Nearest),
            sys::wlr_scale_filter_mode::WLR_SCALE_FILTER_NEAREST
        );
    }

    /// The defaults must be the values wlroots reads out of a zeroed options
    /// struct, or a builder that sets nothing would not mean "no options".
    #[test]
    fn the_default_modes_are_wlroots_zero_values() {
        assert_eq!(sys::wlr_render_blend_mode::from(BlendMode::default()).0, 0);
        assert_eq!(sys::wlr_scale_filter_mode::from(FilterMode::default()).0, 0);
    }

    #[test]
    fn render_color_round_trips_through_an_array() {
        let c = RenderColor::from([0.25, 0.5, 0.75, 1.0]);
        assert_eq!(c, RenderColor::new(0.25, 0.5, 0.75, 1.0));
        assert_eq!(<[f32; 4]>::from(c), [0.25, 0.5, 0.75, 1.0]);
    }

    /// Pass options with nothing set must be four zeroes, since that is what
    /// `wlr_renderer_begin_buffer_pass` substitutes for a null pointer.
    #[test]
    fn default_pass_options_are_the_zeroed_struct() {
        let raw = BufferPassOptions::new().to_sys();
        assert!(raw.timer.is_null());
        assert!(raw.color_transform.is_null());
        assert!(raw.signal_timeline.is_null());
        assert_eq!(raw.signal_point, 0);
    }
}
