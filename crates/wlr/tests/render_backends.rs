//! The backend-specific renderer and texture views.
//!
//! `wlr_pixman_texture_get_image`, `wlr_gles2_texture_get_attribs` and
//! `wlr_vk_texture_get_image_attribs` are all undefined behaviour on an object
//! of the wrong kind, and each ships with a separate `wlr_*_is_*` test the
//! caller is trusted to have run. This crate answers that with view types:
//! `Renderer::as_pixman` and friends return `Some` only when the test passed,
//! and every backend-specific call hangs off the view.
//!
//! What can actually be asserted here is bounded by the machine. CI has no GPU,
//! so the pixman arm is the one that produces a `Some`; the GLES2 and Vulkan
//! arms are exercised in the direction that *is* available everywhere — that a
//! pixman renderer answers `None` to both, which is the discrimination the
//! whole pattern exists to make.

use std::sync::Once;

use wlr::{
    Allocator, Backend, Box2D, BufferCaps, BufferPassOptions, ColorEncoding, ColorPrimaries,
    ColorRange, ColorTransform, Display, DrmFormat, Error, FourCc, Modifier, NamedPrimaries,
    RectOptions, RenderColor, Renderer, TextureOptions, TransferFunction,
};

fn pixman_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: single-threaded, before any other thread exists, and each
        // integration binary is its own process.
        unsafe {
            std::env::set_var("WLR_RENDERER", "pixman");
        }
    });
}

/// The one test here that needs a backend needs it headless, for the same
/// reason `tests/render.rs` does: CI has no DRM node.
fn headless_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: as above.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            std::env::set_var("WLR_RENDERER", "pixman");
        }
    });
}

/// The linear ARGB8888 format the allocator hands out.
fn argb() -> DrmFormat {
    DrmFormat::new(FourCc::ARGB8888, [Modifier::LINEAR])
}

/// Four-by-four ARGB8888 pixels, the smallest texture worth making.
fn pixels() -> Vec<u8> {
    vec![0x80; 4 * 4 * 4]
}

#[test]
fn a_pixman_renderer_answers_the_pixman_view_and_nothing_else() {
    pixman_env();
    let renderer = Renderer::pixman().expect("pixman renderer");

    assert!(
        renderer.as_pixman().is_some(),
        "wlr_renderer_is_pixman must agree with the constructor"
    );

    #[cfg(wlr_has_gles2_renderer)]
    assert!(
        renderer.as_gles2().is_none(),
        "a pixman renderer is not a GLES2 one, and the view is what says so"
    );

    #[cfg(wlr_has_vulkan_renderer)]
    assert!(
        renderer.as_vulkan().is_none(),
        "a pixman renderer is not a Vulkan one"
    );
}

/// The view borrows the renderer, so it is the renderer's own liveness that
/// makes every accessor on it sound. Nothing here needs to assert that — it is
/// a compile-time property — but the accessors do have to work.
#[test]
fn a_pixman_texture_exposes_its_pixman_image() {
    pixman_env();
    let renderer = Renderer::pixman().expect("pixman renderer");
    let data = pixels();
    let texture = renderer
        .texture_from_pixels(FourCc::ARGB8888, 4 * 4, 4, 4, &data)
        .expect("texture");

    // SAFETY: the image is borrowed from the texture and only compared against
    // null here — it is never dereferenced and never unref'd, which is the
    // whole of the contract.
    let image = unsafe { texture.pixman_image() };
    assert!(
        image.is_some(),
        "every texture the pixman renderer makes is backed by a pixman_image_t"
    );

    // SAFETY: as above.
    let again = unsafe { texture.pixman_image() };
    assert_eq!(image, again, "the texture owns one image, not one per call");
}

/// `wlr_pixman_renderer_get_buffer_image` is a "get" that *creates*: on a cache
/// miss it calls `create_buffer` (`render/pixman/renderer.c`). So it answers
/// `Some` for a buffer nothing has rendered into yet, and the same image
/// afterwards.
///
/// Pinned because the name says the opposite, and a caller who read it as a
/// cache lookup would use `None` to mean "not yet drawn" and get it wrong every
/// time.
#[test]
fn a_pixman_renderers_buffer_image_is_created_on_demand_not_looked_up() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");

    let pixman = renderer.as_pixman().expect("pixman view");
    // SAFETY: borrowed from the renderer's cache, compared against null and
    // against itself — never dereferenced, never unref'd.
    let before = unsafe { pixman.buffer_image(&buffer) };
    assert!(
        before.is_some(),
        "the accessor creates the entry rather than reporting its absence"
    );

    {
        let mut pass = renderer
            .begin_buffer_pass(&buffer, &BufferPassOptions::new())
            .expect("pass");
        pass.add_rect(&RectOptions::new(
            Box2D::default(),
            RenderColor::new(0.0, 0.0, 1.0, 1.0),
        ))
        .expect("fill");
    }

    // SAFETY: as above.
    let after = unsafe { pixman.buffer_image(&buffer) };
    assert_eq!(
        before, after,
        "one cache entry per buffer, reused rather than remade"
    );
}

/// The GLES2 accessors have to *exist* and answer `None` for a pixman
/// renderer's texture. That is the assertion CI can make without a GPU, and it
/// is not vacuous: it is exactly the type test that stops
/// `wlr_gles2_texture_get_attribs` being called on a pixman texture.
#[cfg(wlr_has_gles2_renderer)]
#[test]
fn the_gles2_texture_accessor_refuses_a_pixman_texture() {
    pixman_env();
    let renderer = Renderer::pixman().expect("pixman renderer");
    let data = pixels();
    let texture = renderer
        .texture_from_pixels(FourCc::ARGB8888, 4 * 4, 4, 4, &data)
        .expect("texture");

    assert!(texture.gles2_attribs().is_none());
}

/// As above, for Vulkan. Note there are two accessors here and only one for
/// GLES2: `wlr_vk_texture_has_alpha` is a separate call with the same
/// precondition, so it gets the same `Option`.
#[cfg(wlr_has_vulkan_renderer)]
#[test]
fn the_vulkan_texture_accessors_refuse_a_pixman_texture() {
    pixman_env();
    let renderer = Renderer::pixman().expect("pixman renderer");
    let data = pixels();
    let texture = renderer
        .texture_from_pixels(FourCc::ARGB8888, 4 * 4, 4, 4, &data)
        .expect("texture");

    assert!(texture.vulkan_attribs().is_none());
    assert!(texture.vulkan_has_alpha().is_none());
}

/// The GL target constants are ABI a consumer writes shaders against, and this
/// crate writes them out by hand rather than taking a GL dependency. Pinning
/// them here is cheap and the alternative is a silent mismatch.
#[cfg(wlr_has_gles2_renderer)]
#[test]
fn the_gl_texture_targets_are_the_values_gl_defines() {
    use wlr::Gles2TextureAttribs;

    assert_eq!(Gles2TextureAttribs::TARGET_2D, 0x0DE1);
    assert_eq!(Gles2TextureAttribs::TARGET_EXTERNAL_OES, 0x8D65);
}

/// The pixman renderer has no GPU timer at all, which is what makes
/// `create_timer` fail — and so `RenderTimer::is_gles2` unreachable from here.
/// Asserted rather than left implicit, because a renderer that started
/// answering `Some` would change what the rest of the render tests mean.
#[test]
fn the_pixman_renderer_has_no_timer() {
    pixman_env();
    let renderer = Renderer::pixman().expect("pixman renderer");
    assert!(renderer.create_timer().is_err());
}

/// The colour-encoding mask is read off the renderer, and the pixman renderer
/// converts no YCbCr at all — so the honest assertion is that the accessor
/// reports a set rather than a single value, and reports the renderer's own.
#[test]
fn a_renderer_reports_its_colour_encodings_as_a_set() {
    pixman_env();
    let renderer = Renderer::pixman().expect("pixman renderer");
    let encodings = renderer.color_encodings();

    // Whatever wlroots filled in, the mask must round-trip through `bits`.
    assert_eq!(wlr::ColorEncodings::from_bits(encodings.bits()), encodings);
    assert_eq!(
        renderer.buffer_caps(),
        BufferCaps::DATA_PTR,
        "still the pixman renderer"
    );
}

/// wlroots **ignores** a colour transform its renderer cannot apply: the pass
/// is begun, the drawing happens, and the colours are simply wrong. That turns
/// a colour-managed compositor into a silently mis-rendering one, so this crate
/// refuses instead.
///
/// The pixman renderer is the case that makes this testable — it sets
/// `output_color_transform` false explicitly.
#[test]
fn a_pass_refuses_a_colour_transform_the_renderer_cannot_apply() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");

    assert!(
        !renderer.features().output_color_transform,
        "the premise of this test"
    );

    let transform =
        ColorTransform::inverse_eotf(TransferFunction::Srgb).expect("inverse eotf transform");
    let options = BufferPassOptions::new().color_transform(&transform);
    assert_eq!(
        renderer.begin_buffer_pass(&buffer, &options).err(),
        Some(Error::Operation("wlr_renderer_begin_buffer_pass"))
    );

    // Without the transform the very same pass is fine, which is what proves
    // the refusal is about the transform and not about the buffer.
    renderer
        .begin_buffer_pass(&buffer, &BufferPassOptions::new())
        .expect("pass");
}

/// The same argument on the input side: a texture tagged with a transfer
/// function a renderer cannot honour is drawn untagged rather than refused.
#[test]
fn a_textured_draw_refuses_a_colour_tag_the_renderer_cannot_apply() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");

    assert!(
        !renderer.features().input_color_transform,
        "the premise of this test"
    );

    let data = pixels();
    let texture = renderer
        .texture_from_pixels(FourCc::ARGB8888, 4 * 4, 4, 4, &data)
        .expect("texture");

    let mut pass = renderer
        .begin_buffer_pass(&buffer, &BufferPassOptions::new())
        .expect("pass");

    // An untagged draw is accepted...
    pass.add_texture(&TextureOptions::new(&texture))
        .expect("untagged draw");

    // ...and each of the five colour tags is refused on its own, so a future
    // edit cannot let one through unnoticed.
    let tagged = [
        TextureOptions::new(&texture).transfer_function(TransferFunction::Srgb),
        TextureOptions::new(&texture).primaries(ColorPrimaries::named(NamedPrimaries::Srgb)),
        TextureOptions::new(&texture).color_encoding(ColorEncoding::Bt709),
        TextureOptions::new(&texture).color_range(ColorRange::Limited),
        TextureOptions::new(&texture).luminance_multiplier(2.0),
    ];
    for options in &tagged {
        assert_eq!(
            pass.add_texture(options),
            Err(Error::Operation("wlr_render_pass_add_texture")),
            "{options:?}"
        );
    }
}

/// `ColorTransform` is reference-counted and borrows nothing, so it may outlive
/// every renderer that ever saw it. Asserted because the obvious alternative
/// design — tying it to a renderer — would have been wrong, and because a
/// transform freed with its renderer would fail here rather than silently.
#[test]
fn a_colour_transform_outlives_every_renderer() {
    pixman_env();
    let transform = {
        let renderer = Renderer::pixman().expect("pixman renderer");
        let transform =
            ColorTransform::inverse_eotf(TransferFunction::Srgb).expect("inverse eotf transform");
        // The renderer is what the transform would have borrowed under a
        // different design; it is dropped here.
        drop(renderer);
        transform
    };
    let out = transform.eval([0.5, 0.5, 0.5]);
    assert!(out[0] > 0.5, "still evaluable: {out:?}");

    // And a clone taken afterwards still names the same object.
    assert_eq!(transform.clone().as_ptr(), transform.as_ptr());
}

/// The view types are borrows of a renderer that is `!Send`, and they must not
/// be able to escape that.
#[test]
fn the_backend_views_stay_on_one_thread() {
    use static_assertions::assert_not_impl_any;

    assert_not_impl_any!(wlr::Pixman<'static>: Send, Sync);
    assert_not_impl_any!(wlr::Egl: Send, Sync);
}
