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
use super::color::{ColorEncoding, ColorPrimaries, ColorRange, ColorTransform, TransferFunction};
use super::sync::SyncTimeline;
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
/// zeroed `wlr_buffer_pass_options`: no timer, no colour transform, no signal
/// timeline.
#[derive(Debug, Default)]
pub struct BufferPassOptions<'a> {
    timer: Option<&'a RenderTimer<'a>>,
    color_transform: Option<&'a ColorTransform>,
    signal: Option<(&'a SyncTimeline, u64)>,
}

impl<'a> BufferPassOptions<'a> {
    /// The default options: no timer, no colour transform, no timeline.
    pub fn new() -> BufferPassOptions<'a> {
        BufferPassOptions {
            timer: None,
            color_transform: None,
            signal: None,
        }
    }

    /// Time this pass with `timer`.
    ///
    /// The pass keeps the pointer and writes through it when it is submitted
    /// (`render/gles2/pass.c` reads `pass->timer` from inside `submit`), so the
    /// timer has to outlive the pass and wlroots does not check that it does.
    /// [`Renderer::begin_buffer_pass`](crate::Renderer::begin_buffer_pass)
    /// takes these options at the *pass's* lifetime rather than the call's,
    /// which is what turns "has to outlive" into a compile error —
    /// `tests/ui/timer_outlives_pass.rs` pins it.
    #[must_use]
    pub fn timer(mut self, timer: &'a RenderTimer<'a>) -> Self {
        self.timer = Some(timer);
        self
    }

    /// The timer these options carry, for
    /// [`Renderer::begin_buffer_pass`](crate::Renderer::begin_buffer_pass)'s
    /// identity check.
    pub(crate) fn timer_ref(&self) -> Option<&RenderTimer<'a>> {
        self.timer
    }

    /// Apply `transform` to everything this pass draws, on the way out.
    ///
    /// Requires a renderer whose
    /// [`features().output_color_transform`](crate::RendererFeatures::output_color_transform)
    /// is set; the pixman renderer's is not.
    /// [`Renderer::begin_buffer_pass`](crate::Renderer::begin_buffer_pass)
    /// refuses rather than letting wlroots ignore it silently.
    ///
    /// The transform is borrowed only for the pass — it is reference-counted
    /// and independent of any renderer, so the same one may be reused across
    /// passes and outlive them all.
    #[must_use]
    pub fn color_transform(mut self, transform: &'a ColorTransform) -> Self {
        self.color_transform = Some(transform);
        self
    }

    /// Signal `point` on `timeline` once the GPU has finished this pass.
    ///
    /// Requires a renderer whose
    /// [`features().timeline`](crate::RendererFeatures::timeline) is set —
    /// which `WLR_RENDER_NO_EXPLICIT_SYNC=1` in the environment turns off, and
    /// the pixman renderer never has.
    /// [`Renderer::begin_buffer_pass`](crate::Renderer::begin_buffer_pass)
    /// refuses rather than dropping the request.
    #[must_use]
    pub fn signal(mut self, timeline: &'a SyncTimeline, point: u64) -> Self {
        self.signal = Some((timeline, point));
        self
    }

    /// Whether these options name anything the renderer has to support.
    ///
    /// wlroots ignores a colour transform or a signal timeline that its
    /// renderer cannot honour, which turns a colour-managed or explicitly
    /// synchronised compositor into a silently wrong one.
    pub(crate) fn unsupported_by(&self, features: &super::RendererFeatures) -> bool {
        (self.color_transform.is_some() && !features.output_color_transform)
            || (self.signal.is_some() && !features.timeline)
    }

    pub(crate) fn to_sys(&self) -> sys::wlr_buffer_pass_options {
        sys::wlr_buffer_pass_options {
            timer: self
                .timer
                .map_or(std::ptr::null_mut(), |timer| timer.as_ptr()),
            color_transform: self
                .color_transform
                .map_or(std::ptr::null_mut(), ColorTransform::as_ptr),
            signal_timeline: self
                .signal
                .map_or(std::ptr::null_mut(), |(timeline, _)| timeline.as_ptr()),
            signal_point: self.signal.map_or(0, |(_, point)| point),
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
    /// The renderer that created this timer.
    ///
    /// `wlr_render_timer` is `{ impl }` and nothing else — unlike
    /// `wlr_texture`, which carries a `renderer` back-pointer that
    /// [`RenderPass::add_texture`] uses to refuse a texture from elsewhere.
    /// Without recording it here there is no way to make the same check, and
    /// `PhantomData<&'r ()>` proves only that *some* renderer outlives this
    /// timer, not that it is the one being handed it.
    ///
    /// That mattered because a pass downcasts the timer to its own renderer's
    /// private timer type at submit. Two live renderers of different kinds are
    /// constructible here, so a GLES2 timer could reach a Vulkan pass and be
    /// reinterpreted as a Vulkan one — a cross-type reinterpretation of a heap
    /// object, reached with no `unsafe` at the call site.
    renderer: *mut sys::wlr_renderer,
    _renderer: PhantomData<&'r ()>,
}

impl<'r> RenderTimer<'r> {
    /// The renderer that created this timer, for the identity check
    /// [`Renderer::begin_buffer_pass`](super::Renderer::begin_buffer_pass)
    /// makes before accepting one.
    pub(crate) fn renderer_ptr(&self) -> *mut sys::wlr_renderer {
        self.renderer
    }

    /// # Safety
    ///
    /// `raw` must be a live timer this value takes ownership of, created by
    /// `renderer`, which is the renderer `'r` borrows.
    pub(crate) unsafe fn from_raw(
        raw: *mut sys::wlr_render_timer,
        renderer: *mut sys::wlr_renderer,
    ) -> RenderTimer<'r> {
        RenderTimer {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_render_timer"),
            renderer,
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

    /// Whether this timer belongs to the GLES2 renderer.
    ///
    /// There is no Vulkan counterpart. wlroots ships `wlr_render_timer_is_gles2`
    /// and no `wlr_render_timer_is_vk`, so this is an asymmetry in the C API
    /// rather than an omission here.
    #[cfg(wlr_has_gles2_renderer)]
    pub fn is_gles2(&self) -> bool {
        // SAFETY: this value owns a live timer for as long as it exists.
        unsafe { sys::wlr_render_timer_is_gles2(self.raw.as_ptr()) }
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
    /// `None` is wlroots' unset, which is the integer 0 — **not** a value of
    /// `wlr_color_transfer_function`, whose variants all start at `1 << 0`.
    transfer_function: Option<TransferFunction>,
    /// Stored by value rather than borrowed, for the same reason as `alpha`:
    /// wlroots takes a `const wlr_color_primaries *` and a pointer to a
    /// temporary dangles by the time the call reads it.
    primaries: Option<ColorPrimaries>,
    color_encoding: ColorEncoding,
    color_range: ColorRange,
    luminance_multiplier: Option<f32>,
    wait: Option<(&'a SyncTimeline, u64)>,
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
            transfer_function: None,
            primaries: None,
            color_encoding: ColorEncoding::None,
            color_range: ColorRange::None,
            luminance_multiplier: None,
            wait: None,
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

    /// The transfer function the texture's pixels are encoded with.
    ///
    /// Unset by default, which wlroots reads as "assume the renderer's own".
    /// Setting one requires a renderer whose
    /// [`features().input_color_transform`](crate::RendererFeatures::input_color_transform)
    /// is set — [`RenderPass::add_texture`] refuses otherwise rather than
    /// letting wlroots ignore it.
    #[must_use]
    pub fn transfer_function(mut self, tf: TransferFunction) -> Self {
        self.transfer_function = Some(tf);
        self
    }

    /// The colour primaries the texture's pixels are in.
    ///
    /// Taken by value: wlroots reads this through a `const` pointer during the
    /// draw, and this crate keeps the storage it points at inside these
    /// options — the same arrangement `alpha` uses.
    #[must_use]
    pub fn primaries(mut self, primaries: ColorPrimaries) -> Self {
        self.primaries = Some(primaries);
        self
    }

    /// The YCbCr matrix coefficients to convert the texture with.
    ///
    /// [`ColorEncoding::None`] — the default — means the texture is already
    /// RGB, or that the encoding is unknown. Must be one of the encodings
    /// [`Renderer::color_encodings`](crate::Renderer::color_encodings) reports.
    #[must_use]
    pub fn color_encoding(mut self, encoding: ColorEncoding) -> Self {
        self.color_encoding = encoding;
        self
    }

    /// Whether the texture's values use the full or the limited range.
    #[must_use]
    pub fn color_range(mut self, range: ColorRange) -> Self {
        self.color_range = range;
        self
    }

    /// Scale the texture's luminance by this factor.
    ///
    /// Stored inline for the same reason as `alpha`.
    #[must_use]
    pub fn luminance_multiplier(mut self, multiplier: f32) -> Self {
        self.luminance_multiplier = Some(multiplier);
        self
    }

    /// Wait for `point` on `timeline` before sampling the texture.
    ///
    /// Requires a renderer whose
    /// [`features().timeline`](crate::RendererFeatures::timeline) is set;
    /// [`RenderPass::add_texture`] refuses otherwise, because wlroots would
    /// simply not wait and the tearing that follows is intermittent.
    #[must_use]
    pub fn wait(mut self, timeline: &'a SyncTimeline, point: u64) -> Self {
        self.wait = Some((timeline, point));
        self
    }

    /// Whether these options name anything the renderer has to support.
    ///
    /// The YCbCr pair is checked separately from the rest of the colour
    /// options, because they are advertised by a different thing and one of
    /// them can abort the process.
    ///
    /// `input_color_transform` governs transfer functions, primaries and the
    /// luminance multiplier. It says nothing about YCbCr: a renderer
    /// advertises those through the `color_encodings` **mask**, and the two
    /// disagree in both directions on the renderers shipped today — Vulkan
    /// has `input_color_transform = true` with only `BT709|BT601|BT2020` in
    /// its mask, GLES2 has the transform off and an empty mask.
    ///
    /// Gating the pair on `input_color_transform` therefore let a Vulkan
    /// renderer through with, say, `ColorEncoding::Bt709`, and
    /// `setup_get_or_create_pipeline` in wlroots' Vulkan backend asserts
    /// `encoding == NONE || encoding == IDENTITY` and
    /// `range == NONE || range == FULL` for a pipeline key that is not YCbCr.
    /// Both strings are compiled into the shipped `libwlroots-0.20.so`, so
    /// that is a process abort out of a safe call.
    ///
    /// The narrow rule would be "allow a real encoding only for a YCbCr
    /// texture", which this crate cannot express: `wlr_texture` carries
    /// `impl`, `width`, `height` and `renderer` and no format, so there is no
    /// way to ask a [`Texture`](super::Texture) whether it is YCbCr — and one
    /// genuinely can be, via
    /// [`texture_from_dmabuf`](super::Renderer::texture_from_dmabuf). So the
    /// check mirrors the assertion instead and refuses what wlroots would
    /// abort on. That over-refuses for a YCbCr texture, which is the safe
    /// direction: a refusal is an `Err` a compositor can handle, and an
    /// assertion is a process that is gone. Exposing the texture's format
    /// would let this narrow to the exact rule.
    fn unsupported_by(
        &self,
        features: &super::RendererFeatures,
        encodings: super::ColorEncodings,
    ) -> bool {
        let colour = self.transfer_function.is_some()
            || self.primaries.is_some()
            || self.luminance_multiplier.is_some();
        if colour && !features.input_color_transform {
            return true;
        }
        if self.wait.is_some() && !features.timeline {
            return true;
        }
        // The renderer must advertise the encoding at all...
        if self.color_encoding != ColorEncoding::None && !encodings.contains(self.color_encoding) {
            return true;
        }
        // ...and it must be one the non-YCbCr pipeline key accepts, since we
        // cannot prove the texture is YCbCr.
        if !matches!(
            self.color_encoding,
            ColorEncoding::None | ColorEncoding::Identity
        ) {
            return true;
        }
        !matches!(self.color_range, ColorRange::None | ColorRange::Full)
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
    ///   which wlroots also asserts, or if the options name a colour tag or a
    ///   wait timeline this renderer does not support. wlroots ignores both
    ///   silently, which turns a colour-managed or explicitly synchronised
    ///   compositor into a subtly wrong one rather than a failing one.
    pub fn add_texture(&mut self, options: &TextureOptions<'_>) -> Result<()> {
        if options.texture.renderer_ptr() != self.renderer.as_ptr() {
            return Err(Error::Mismatch("RenderPass::add_texture"));
        }
        if !options.src_box_in_bounds() {
            return Err(Error::Operation("wlr_render_pass_add_texture"));
        }
        if options.unsupported_by(&self.renderer.features(), self.renderer.color_encodings()) {
            return Err(Error::Operation("wlr_render_pass_add_texture"));
        }

        // Held in locals for the duration of the call: wlroots reads through
        // these pointers while `add_texture` runs and never retains them.
        let alpha = options.alpha;
        let luminance_multiplier = options.luminance_multiplier;
        let primaries = options.primaries;
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
            // The literal integer 0 is "unset" here, and it is **not** a named
            // variant: `wlr_color_transfer_function`'s values all start at
            // `1 << 0`, so there is nothing in the enum to write instead. The
            // other three do have a zero variant, and use it.
            transfer_function: options
                .transfer_function
                .map_or(sys::wlr_color_transfer_function(0), Into::into),
            primaries: primaries
                .as_ref()
                .map_or(std::ptr::null(), ColorPrimaries::as_c),
            color_encoding: options.color_encoding.into(),
            color_range: options.color_range.into(),
            luminance_multiplier: luminance_multiplier
                .as_ref()
                .map_or(std::ptr::null(), |m| &raw const *m),
            wait_timeline: options
                .wait
                .map_or(std::ptr::null_mut(), |(timeline, _)| timeline.as_ptr()),
            wait_point: options.wait.map_or(0, |(_, point)| point),
        };
        // SAFETY: the pass is live (this value owns it until `submit`), the
        // texture is live and belongs to this renderer (checked above), the
        // clip region and the `alpha`, `luminance_multiplier` and `primaries`
        // locals all outlive the call, the wait timeline is borrowed by the
        // options for at least as long, and the source box satisfies wlroots'
        // own assertion.
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
