//! The render pipeline, end to end, against a real wlroots.
//!
//! Everything here runs on the pixman renderer, which needs no GPU: `pixman`
//! is forced through `WLR_RENDERER` so that
//! [`Renderer::autocreate`](wlr::Renderer::autocreate) picks the same
//! implementation on a developer's workstation and on a CI container with no
//! DRM node at all. What is being tested is this crate's wrappers, not
//! wlroots' renderer selection.
//!
//! The path exercised is the whole of it: a renderer, an allocator built on it,
//! a buffer from that allocator, a pass drawing into the buffer, and the pixels
//! read back out through a texture.

use std::sync::Once;

use wlr::{
    Allocator, Backend, Box2D, BufferCaps, BufferPassOptions, Display, DrmFormat, DrmFormatSet,
    Error, FBox, FourCc, Modifier, ReadPixels, RectOptions, Region, RenderColor, Renderer, Runtime,
    SWAPCHAIN_CAP, Swapchain, TextureOptions,
};

/// `wlr_shm_allocator` hands out `DRM_FORMAT_ARGB8888` buffers whose bytes are
/// B, G, R, A in memory order: the DRM name is read most-significant-byte
/// first, the word is stored little-endian.
const RED: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
const BLUE: [u8; 4] = [0xff, 0x00, 0x00, 0xff];

fn headless_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: single-threaded, before any other thread exists, and each
        // integration binary is its own process.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            std::env::set_var("WLR_RENDERER", "pixman");
        }
    });
}

/// The linear ARGB8888 format every test here allocates in.
fn argb() -> DrmFormat {
    DrmFormat::new(FourCc::ARGB8888, [Modifier::LINEAR])
}

#[test]
fn a_pixman_renderer_needs_no_backend_and_reports_its_capabilities() {
    let renderer = Renderer::pixman().expect("pixman renderer");

    assert_eq!(renderer.buffer_caps(), BufferCaps::DATA_PTR);
    assert!(renderer.drm_fd().is_none(), "software rendering, no device");
    assert!(!renderer.is_lost());

    let features = renderer.features();
    assert!(
        !features.output_color_transform,
        "the pixman renderer sets this false explicitly"
    );

    let formats = renderer
        .texture_formats(BufferCaps::DATA_PTR)
        .expect("pixman samples from mappable buffers");
    assert!(!formats.is_empty());
    assert!(
        formats.get(FourCc::ARGB8888).is_some(),
        "ARGB8888 is the format the rest of this file allocates in"
    );
    assert!(
        renderer.texture_formats(BufferCaps::DMABUF).is_none(),
        "pixman samples from nothing but mappable buffers"
    );
}

/// Pixels in, pixels out: the round trip that proves `texture_from_pixels`
/// copies what it was given and `read_pixels` reads it back.
#[test]
fn pixels_round_trip_through_a_texture() {
    let renderer = Renderer::pixman().expect("pixman renderer");

    let mut source = Vec::new();
    for _ in 0..4 * 4 {
        source.extend_from_slice(&RED);
    }
    let texture = renderer
        .texture_from_pixels(FourCc::ARGB8888, 4 * 4, 4, 4, &source)
        .expect("texture from pixels");
    assert_eq!(texture.width(), 4);
    assert_eq!(texture.height(), 4);

    let mut read_back = vec![0u8; source.len()];
    let mut options = ReadPixels::new(&mut read_back, FourCc::ARGB8888, 4 * 4);
    texture.read_pixels(&mut options).expect("read pixels");
    assert_eq!(read_back, source);
}

/// The pixel data outlives the caller's slice — see `render/texture.rs`'s own
/// doc on why the texture keeps a copy. Reading after the source is gone would
/// be a use-after-free if it did not.
#[test]
fn a_texture_survives_the_slice_it_was_built_from() {
    let renderer = Renderer::pixman().expect("pixman renderer");

    let texture = {
        let mut source = Vec::new();
        for _ in 0..2 * 2 {
            source.extend_from_slice(&BLUE);
        }
        renderer
            .texture_from_pixels(FourCc::ARGB8888, 2 * 4, 2, 2, &source)
            .expect("texture from pixels")
    };

    let mut read_back = vec![0u8; 2 * 2 * 4];
    let mut options = ReadPixels::new(&mut read_back, FourCc::ARGB8888, 2 * 4);
    texture.read_pixels(&mut options).expect("read pixels");
    assert_eq!(&read_back[..4], &BLUE);
}

#[test]
fn preferred_read_format_is_a_real_fourcc() {
    let renderer = Renderer::pixman().expect("pixman renderer");
    let pixels = vec![0u8; 4 * 4 * 4];
    let texture = renderer
        .texture_from_pixels(FourCc::ARGB8888, 4 * 4, 4, 4, &pixels)
        .expect("texture from pixels");

    let format = texture.preferred_read_format();
    assert_ne!(
        format,
        FourCc::INVALID,
        "a texture that implements read_pixels reports a format"
    );
    // Every DRM fourcc is four printable characters; a zero byte here means the
    // code was read as something other than a fourcc.
    for byte in format.0.to_le_bytes() {
        assert!(byte.is_ascii_graphic(), "{format:?}");
    }
}

/// A source box outside the texture is what `wlr_render_pass_add_texture`
/// asserts on, so it must be refused before the call.
#[test]
fn read_pixels_refuses_a_destination_that_is_too_small() {
    let renderer = Renderer::pixman().expect("pixman renderer");
    let pixels = vec![0u8; 4 * 4 * 4];
    let texture = renderer
        .texture_from_pixels(FourCc::ARGB8888, 4 * 4, 4, 4, &pixels)
        .expect("texture from pixels");

    let mut too_small = vec![0u8; 4 * 4 * 4 - 1];
    let mut options = ReadPixels::new(&mut too_small, FourCc::ARGB8888, 4 * 4);
    assert_eq!(
        texture.read_pixels(&mut options),
        Err(Error::Operation("wlr_texture_read_pixels"))
    );
}

/// A stride shorter than one row of pixels is the case `rows * stride` gets
/// wrong: wlroots lays an image `width` pixels wide over the destination and
/// writes a full row from every stride-spaced offset, so the last row runs off
/// the end of a slice that looked big enough. Measured before it was fixed —
/// this exact call wrote 28 bytes past `too_narrow`.
#[test]
fn read_pixels_refuses_a_stride_shorter_than_one_row() {
    let renderer = Renderer::pixman().expect("pixman renderer");
    let pixels = vec![0xffu8; 8 * 8 * 4];
    let texture = renderer
        .texture_from_pixels(FourCc::ARGB8888, 8 * 4, 8, 8, &pixels)
        .expect("texture from pixels");

    let mut too_narrow = vec![0u8; 8 * 4];
    let mut options = ReadPixels::new(&mut too_narrow, FourCc::ARGB8888, 4);
    assert_eq!(
        texture.read_pixels(&mut options),
        Err(Error::Operation("wlr_texture_read_pixels"))
    );
}

/// The same hazard on the way in: a short stride makes the renderer read a full
/// row past the end of the pixels this crate copied for it.
#[test]
fn texture_from_pixels_refuses_a_stride_shorter_than_one_row() {
    let renderer = Renderer::pixman().expect("pixman renderer");
    let pixels = vec![0u8; 8 * 8 * 4];
    assert_eq!(
        renderer
            .texture_from_pixels(FourCc::ARGB8888, 8 * 2, 8, 8, &pixels)
            .err(),
        Some(Error::Operation("Renderer::texture_from_pixels"))
    );
}

#[test]
fn texture_from_pixels_refuses_a_short_slice_and_a_zero_dimension() {
    let renderer = Renderer::pixman().expect("pixman renderer");
    let pixels = vec![0u8; 4 * 4 * 4 - 1];

    assert_eq!(
        renderer
            .texture_from_pixels(FourCc::ARGB8888, 4 * 4, 4, 4, &pixels)
            .err(),
        Some(Error::Operation("Renderer::texture_from_pixels"))
    );
    assert_eq!(
        renderer
            .texture_from_pixels(FourCc::ARGB8888, 0, 4, 4, &pixels)
            .err(),
        Some(Error::Operation("Renderer::texture_from_pixels"))
    );
    assert_eq!(
        renderer
            .texture_from_pixels(FourCc::ARGB8888, 4 * 4, 4, 0, &pixels)
            .err(),
        Some(Error::Operation("Renderer::texture_from_pixels"))
    );
}

/// The headless backend advertises every buffer capability, so pairing it with
/// the pixman renderer gets the shared-memory allocator — mappable buffers a
/// pixman pass can draw into.
#[test]
fn an_allocator_hands_out_buffers_a_pass_can_draw_into() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    assert!(!allocator.buffer_caps().is_empty());

    let buffer = allocator.create_buffer(8, 8, &argb()).expect("buffer");
    assert_eq!(buffer.width(), 8);
    assert_eq!(buffer.height(), 8);

    let mut pass = renderer
        .begin_buffer_pass(&buffer, &BufferPassOptions::new())
        .expect("pass");
    // An empty box means the whole buffer, which is how wlroots reads a zeroed
    // `wlr_box`.
    pass.add_rect(&RectOptions::new(
        Box2D::default(),
        RenderColor::new(1.0, 0.0, 0.0, 1.0),
    ))
    .expect("rect");
    pass.submit().expect("submit");

    let texture = renderer
        .texture_from_buffer(&buffer)
        .expect("texture from the buffer just drawn into");
    let mut read_back = vec![0u8; 8 * 8 * 4];
    let mut options = ReadPixels::new(&mut read_back, FourCc::ARGB8888, 8 * 4);
    texture.read_pixels(&mut options).expect("read pixels");

    for pixel in read_back.chunks_exact(4) {
        assert_eq!(pixel, RED, "the whole buffer should be the rect's colour");
    }
}

/// A clip region is the one render-pass parameter that goes through this
/// crate's own pixman bindings, so it gets its own end-to-end check.
#[test]
fn a_clipped_rect_leaves_the_rest_of_the_buffer_alone() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");

    {
        let mut pass = renderer
            .begin_buffer_pass(&buffer, &BufferPassOptions::new())
            .expect("pass");
        pass.add_rect(&RectOptions::new(
            Box2D::default(),
            RenderColor::new(0.0, 0.0, 1.0, 1.0),
        ))
        .expect("blue background");
        let clip = Region::from_box(Box2D::new(0, 0, 2, 4));
        pass.add_rect(
            &RectOptions::new(Box2D::default(), RenderColor::new(1.0, 0.0, 0.0, 1.0)).clip(&clip),
        )
        .expect("clipped red");
    }

    let texture = renderer.texture_from_buffer(&buffer).expect("texture");
    let mut read_back = vec![0u8; 4 * 4 * 4];
    let mut options = ReadPixels::new(&mut read_back, FourCc::ARGB8888, 4 * 4);
    texture.read_pixels(&mut options).expect("read pixels");

    for row in read_back.chunks_exact(4 * 4) {
        assert_eq!(&row[0..4], RED, "left half is inside the clip");
        assert_eq!(&row[8..12], BLUE, "right half is outside it");
    }
}

/// Drawing a texture is the other half of a pass, and the source box is the
/// parameter wlroots asserts on rather than checking.
#[test]
fn a_textured_draw_lands_where_it_was_asked_to() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");

    let mut source = Vec::new();
    for _ in 0..2 * 2 {
        source.extend_from_slice(&RED);
    }
    let texture = renderer
        .texture_from_pixels(FourCc::ARGB8888, 2 * 4, 2, 2, &source)
        .expect("texture");

    {
        let mut pass = renderer
            .begin_buffer_pass(&buffer, &BufferPassOptions::new())
            .expect("pass");
        pass.add_rect(&RectOptions::new(
            Box2D::default(),
            RenderColor::new(0.0, 0.0, 1.0, 1.0),
        ))
        .expect("blue background");
        pass.add_texture(&TextureOptions::new(&texture).dst_box(Box2D::new(0, 0, 2, 2)))
            .expect("textured draw");

        // A source box reaching past the texture is what wlroots asserts on.
        assert_eq!(
            pass.add_texture(&TextureOptions::new(&texture).src_box(FBox::new(0.0, 0.0, 4.0, 4.0))),
            Err(Error::Operation("wlr_render_pass_add_texture"))
        );
    }

    let readback_texture = renderer.texture_from_buffer(&buffer).expect("texture");
    let mut read_back = vec![0u8; 4 * 4 * 4];
    let mut options = ReadPixels::new(&mut read_back, FourCc::ARGB8888, 4 * 4);
    readback_texture
        .read_pixels(&mut options)
        .expect("read pixels");

    assert_eq!(&read_back[0..4], RED, "the quad's own corner");
    assert_eq!(&read_back[8..12], BLUE, "beyond the quad");
}

/// A texture from one renderer handed to another renderer's pass trips a
/// wlroots assertion — an abort, not a failure — so the wrapper checks first.
#[test]
fn a_texture_from_another_renderer_is_refused_rather_than_aborting() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let other = Renderer::pixman().expect("second pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");

    let pixels = vec![0u8; 2 * 2 * 4];
    let foreign = other
        .texture_from_pixels(FourCc::ARGB8888, 2 * 4, 2, 2, &pixels)
        .expect("texture");

    let mut pass = renderer
        .begin_buffer_pass(&buffer, &BufferPassOptions::new())
        .expect("pass");
    assert_eq!(
        pass.add_texture(&TextureOptions::new(&foreign)),
        Err(Error::Mismatch("RenderPass::add_texture"))
    );
}

/// `wlr_render_pass_add_rect` asserts non-negative extents.
#[test]
fn a_negative_rect_is_refused_rather_than_aborting() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");

    let mut pass = renderer
        .begin_buffer_pass(&buffer, &BufferPassOptions::new())
        .expect("pass");
    assert_eq!(
        pass.add_rect(&RectOptions::new(
            Box2D::new(0, 0, -1, 4),
            RenderColor::default()
        )),
        Err(Error::Operation("wlr_render_pass_add_rect"))
    );
}

#[test]
fn an_allocator_refuses_a_non_positive_size() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    assert_eq!(
        allocator.create_buffer(0, 8, &argb()).err(),
        Some(Error::Operation("Allocator::create_buffer"))
    );
    assert_eq!(
        allocator.create_buffer(8, -1, &argb()).err(),
        Some(Error::Operation("Allocator::create_buffer"))
    );
}

/// The ring is exactly [`SWAPCHAIN_CAP`] deep, and a slot comes back the moment
/// the buffer holding it is unlocked.
#[test]
fn a_swapchain_holds_exactly_four_buffers_in_flight() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    let swapchain = Swapchain::create(&allocator, 16, 16, &argb()).expect("swapchain");
    assert_eq!(swapchain.width(), 16);
    assert_eq!(swapchain.height(), 16);
    assert_eq!(swapchain.format().format(), FourCc::ARGB8888);
    assert!(swapchain.allocator_alive());
    assert_eq!(swapchain.in_flight(), 0);

    let mut held = Vec::new();
    for _ in 0..SWAPCHAIN_CAP {
        held.push(swapchain.acquire().expect("a free slot"));
    }
    assert_eq!(swapchain.in_flight(), SWAPCHAIN_CAP);
    assert!(swapchain.has_buffer(&held[0]));

    assert_eq!(
        swapchain.acquire().err(),
        Some(Error::Operation("wlr_swapchain_acquire")),
        "the ring is full"
    );

    let released = held.pop().expect("four were acquired");
    drop(released);
    assert_eq!(swapchain.in_flight(), SWAPCHAIN_CAP - 1);
    let reacquired = swapchain.acquire().expect("the released slot");
    assert!(swapchain.has_buffer(&reacquired));
}

#[test]
fn a_swapchain_refuses_a_non_positive_size() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    assert_eq!(
        Swapchain::create(&allocator, 0, 16, &argb()).err(),
        Some(Error::Operation("Swapchain::create"))
    );
    assert_eq!(
        Swapchain::create(&allocator, 16, -3, &argb()).err(),
        Some(Error::Operation("Swapchain::create"))
    );
}

/// A buffer from an allocator can be drawn into and then handed to a swapchain
/// query — the producer and consumer references are different types, and both
/// deref to the same read-only `Buffer`.
#[test]
fn producer_and_consumer_references_share_the_read_only_surface() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    let produced = allocator.create_buffer(8, 8, &argb()).expect("buffer");
    let swapchain = Swapchain::create(&allocator, 8, 8, &argb()).expect("swapchain");
    let consumed = swapchain.acquire().expect("a free slot");

    assert_eq!(produced.width(), consumed.width());
    assert!(!swapchain.has_buffer(&produced), "a different buffer");
    assert!(swapchain.has_buffer(&consumed));
    // Shared-memory buffers are not DMA-BUFs, and asking must answer rather
    // than crash.
    assert!(produced.dmabuf().is_none());
}

/// The renderer's own format set is a real `wlr_drm_format_set`, so the set
/// wrappers are exercised against wlroots' data rather than only against a set
/// this crate built.
#[test]
fn the_renderers_format_set_reads_the_same_way_a_built_one_does() {
    let renderer = Renderer::pixman().expect("pixman renderer");
    let formats = renderer
        .texture_formats(BufferCaps::DATA_PTR)
        .expect("pixman samples from mappable buffers");

    let mut rebuilt = DrmFormatSet::new();
    for format in formats.iter() {
        for modifier in format.modifiers() {
            rebuilt.add(format.format(), modifier).expect("add");
        }
    }
    assert_eq!(rebuilt.len(), formats.len());
    for format in formats.iter() {
        for modifier in format.modifiers() {
            assert!(rebuilt.has(format.format(), modifier));
        }
    }

    // Intersecting a set with itself is itself, and intersecting it with a
    // disjoint set is the documented `Err`.
    let intersection = DrmFormatSet::intersect(&rebuilt, &rebuilt).expect("self-intersection");
    assert_eq!(intersection.len(), rebuilt.len());

    let mut disjoint = DrmFormatSet::new();
    disjoint
        .add(FourCc::from_chars(b'Z', b'Z', b'Z', b'Z'), Modifier::LINEAR)
        .expect("add");
    assert_eq!(
        DrmFormatSet::intersect(&rebuilt, &disjoint).err(),
        Some(Error::Operation("wlr_drm_format_set_intersect"))
    );
}

/// `Runtime::init_graphics` creates a renderer and an allocator it never
/// destroys, so what a consumer gets is a **view**: the same queries, no
/// `Drop`, no way to acquire one.
#[test]
fn the_runtimes_renderer_and_allocator_are_reachable_as_views() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");

    assert!(runtime.renderer_ref().is_none(), "before init_graphics");
    assert!(runtime.allocator_ref().is_none(), "before init_graphics");

    runtime.init_graphics(&display, &backend).expect("graphics");

    let renderer = runtime.renderer_ref().expect("after init_graphics");
    assert!(!renderer.buffer_caps().is_empty());
    assert!(renderer.texture_formats(renderer.buffer_caps()).is_some());

    let allocator = runtime.allocator_ref().expect("after init_graphics");
    assert!(!allocator.buffer_caps().is_empty());

    let buffer = allocator
        .create_buffer(8, 8, &argb())
        .expect("the runtime's own allocator allocates");
    assert_eq!(buffer.width(), 8);

    let swapchain =
        Swapchain::create_on_ref(allocator, 8, 8, &argb()).expect("swapchain on the view");
    assert!(swapchain.allocator_alive());
    assert!(swapchain.acquire().is_ok());
}

/// `wlr_shm_allocator` buffers report shared-memory attributes matching what
/// they were created with, and are not DMA-BUFs — the mirror image of
/// `produced.dmabuf().is_none()` above.
#[test]
fn an_shm_allocator_buffer_reports_its_shm_attributes() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    let buffer = allocator.create_buffer(4, 2, &argb()).expect("buffer");
    let shm = buffer.shm().expect("wlr_shm_allocator buffers are shm");
    assert_eq!(shm.format(), FourCc::ARGB8888);
    assert_eq!(shm.width(), 4);
    assert_eq!(shm.height(), 2);
    assert!(shm.stride() >= 4 * 4, "at least 4 bytes per pixel wide");
    assert!(shm.offset() >= 0);
}

/// `begin_data_ptr_access`/the guard's `Drop` round-trip: writing through the
/// mapping and reading it back through a second, later mapping sees the
/// write, and the format/stride the guard reports match what `shm()` itself
/// reports for the same buffer.
#[test]
fn data_ptr_access_writes_are_visible_after_the_guard_is_dropped_and_reopened() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    let buffer = allocator.create_buffer(2, 2, &argb()).expect("buffer");
    let stride = buffer.shm().expect("shm buffer").stride() as usize;

    {
        let mut access = buffer
            .begin_data_ptr_access(wlr::DataPtrAccess::WRITE)
            .expect("no other mapping is open");
        assert_eq!(access.stride(), stride);
        let data = access.data_mut().expect("opened with WRITE");
        data.fill(0xAB);
        assert!(access.data().is_none(), "not opened with READ");
    }

    let access = buffer
        .begin_data_ptr_access(wlr::DataPtrAccess::READ)
        .expect("the previous guard released the mapping on drop");
    let data = access.data().expect("opened with READ");
    assert!(data.iter().all(|&b| b == 0xAB));
}

/// The most direct route to wlroots' own `!accessing_data_ptr` assertion —
/// the one `Buffer::begin_data_ptr_access` stands in front of — is opening a
/// second mapping while the first is still alive. This must return `None`,
/// not abort. (It is not the only route; see
/// `renderer_calls_are_refused_while_a_data_ptr_mapping_is_open`.)
#[test]
fn a_second_data_ptr_access_while_the_first_is_open_is_refused() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    let buffer = allocator.create_buffer(2, 2, &argb()).expect("buffer");
    let _first = buffer
        .begin_data_ptr_access(wlr::DataPtrAccess::READ)
        .expect("first mapping opens");

    assert!(
        buffer
            .begin_data_ptr_access(wlr::DataPtrAccess::READ)
            .is_none(),
        "wlroots' accessing_data_ptr assert must never be reached"
    );
}

/// The *other* route into that same assert, and the one a borrow cannot close:
/// the renderer opens wlroots' own data-pointer bracket on any shared-memory
/// buffer it textures, and every entry point that does takes only a shared
/// `&Buffer` — so a live `BufferDataAccess` guard does not stop the call from
/// compiling. Each of them must refuse while a mapping is open rather than
/// call in and abort the process.
#[test]
fn renderer_calls_are_refused_while_a_data_ptr_mapping_is_open() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    let buffer = allocator.create_buffer(2, 2, &argb()).expect("buffer");
    // A texture made *before* the mapping opens, so that the refusal below is
    // about the mapping and not about the texture being unmakeable.
    let texture = renderer
        .texture_from_buffer(&buffer)
        .expect("texture from a buffer with no mapping open");

    let access = buffer
        .begin_data_ptr_access(wlr::DataPtrAccess::READ)
        .expect("mapping opens");

    assert!(
        renderer.texture_from_buffer(&buffer).is_err(),
        "texturing opens wlroots' own data-ptr bracket"
    );
    assert!(
        renderer
            .begin_buffer_pass(&buffer, &BufferPassOptions::new())
            .is_err(),
        "a pixman pass maps its target through the same bracket"
    );
    assert!(
        texture.update_from_buffer(&buffer, None).is_err(),
        "an update reads the source through the same bracket"
    );
    let pixman = renderer.as_pixman().expect("this is the pixman renderer");
    // SAFETY: the returned image is only ever compared against `None` here; it
    // is never dereferenced, unref'd, or kept.
    assert!(
        unsafe { pixman.buffer_image(&buffer) }.is_none(),
        "creating the pixman cache entry opens the same bracket"
    );

    drop(access);
    assert!(
        renderer.texture_from_buffer(&buffer).is_ok(),
        "and every one of them works again once the guard is gone"
    );
}
