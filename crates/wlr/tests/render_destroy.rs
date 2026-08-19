//! Destruction order for the render types, and the wlroots assertions that
//! punish getting it wrong.
//!
//! Every assertion wlroots makes here is compiled into the distro's
//! `libwlroots-0.20.so` (no `NDEBUG`), so "getting it wrong" means the test
//! process **aborts** rather than fails. A test in this file that starts
//! crashing rather than failing is reporting exactly the bug it was written
//! for.

use std::sync::Once;

use wlr::{
    Allocator, Backend, Box2D, BufferPassOptions, Display, DrmFormat, Error, FourCc, Modifier,
    RectOptions, RenderColor, Renderer, SWAPCHAIN_CAP, Swapchain,
};

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

fn argb() -> DrmFormat {
    DrmFormat::new(FourCc::ARGB8888, [Modifier::LINEAR])
}

/// A renderer this crate created is **not** owned by the backend or the
/// display, and nothing in wlroots destroys it: the only
/// `wlr_renderer_destroy` calls in wlroots 0.20.2 are for the DRM backend's own
/// internal renderer. So a `Renderer` outliving the display it was created
/// alongside is fine, and there is deliberately no
/// `Error::Destroyed("wlr_renderer")` for a consumer to handle.
///
/// This is the test that would catch that premise changing: if wlroots ever did
/// destroy the renderer with the backend, the `drop` at the end would be a
/// double free rather than a clean teardown.
#[test]
fn a_renderer_outlives_the_display_it_was_created_alongside() {
    headless_env();

    let renderer = {
        let display = Display::new().expect("display");
        let backend = Backend::autocreate(&display.event_loop()).expect("backend");
        Renderer::autocreate(&backend).expect("renderer")
    };

    // The display — and with it the backend — is gone. The renderer is still
    // this test's, and still answers.
    assert!(!renderer.is_lost());
    assert!(!renderer.buffer_caps().is_empty());
    drop(renderer);
}

/// `wlr_renderer_destroy` emits `events.destroy` and then asserts that *both*
/// its signal listener lists are empty. This crate links a listener into
/// `events.lost`, so it has to unlink before destroying — this is the test that
/// aborts if that order is ever reversed.
#[test]
fn destroying_many_renderers_does_not_trip_the_listener_assertions() {
    for _ in 0..32 {
        let renderer = Renderer::pixman().expect("pixman renderer");
        drop(renderer);
    }
}

/// `wlr_allocator_destroy` makes the same assertion about its own destroy
/// signal, which is why this crate links no listener into it.
#[test]
fn destroying_many_allocators_does_not_trip_the_listener_assertion() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");

    for _ in 0..16 {
        let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
        drop(allocator);
    }
}

/// wlroots nulls `wlr_swapchain.allocator` from its own listener when the
/// allocator dies, and this crate reports that as
/// `Error::Destroyed("wlr_allocator")` rather than calling into a freed object.
///
/// Reaching the state at all takes a non-owning [`AllocatorRef`]: a
/// [`Swapchain`] built on an owned [`Allocator`] borrows it, so the borrow
/// checker refuses this ordering outright. That is the point — this is the
/// behaviour the *views* rely on.
#[test]
fn a_swapchain_reports_its_allocator_dying() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");

    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    // SAFETY: the allocator is live right now, which is all this view is used
    // for — `Swapchain::create_on_ref` reads it during the call and keeps no
    // Rust-side pointer to it. The view itself is never touched again after
    // the allocator is dropped below.
    let view = unsafe { Allocator::borrow_from_ptr(allocator.as_ptr()) };
    let swapchain = Swapchain::create_on_ref(view, 8, 8, &argb()).expect("swapchain");

    assert!(swapchain.allocator_alive());
    assert!(swapchain.acquire().is_ok());

    drop(allocator);

    assert!(
        !swapchain.allocator_alive(),
        "wlroots nulls the field from its own destroy listener"
    );
    assert_eq!(
        swapchain.acquire().err(),
        Some(Error::Destroyed("wlr_allocator")),
        "acquiring must report the dead allocator, not call into it"
    );
    // Dropping the swapchain here destroys it with a dead allocator, which is
    // the other half of the same claim.
    drop(swapchain);
}

/// A pass that is dropped instead of submitted must still be freed, and must
/// still release the renderer's single-live-pass claim. Sixty-four of them in a
/// row is what makes a leak of either visible: a leaked *pass* would leave the
/// claim set and the next `begin_buffer_pass` would refuse.
#[test]
fn dropping_a_pass_submits_it_and_frees_the_renderer_for_the_next_one() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");

    for _ in 0..64 {
        let mut pass = renderer
            .begin_buffer_pass(&buffer, &BufferPassOptions::new())
            .expect("a pass, every time");
        pass.add_rect(&RectOptions::new(
            Box2D::default(),
            RenderColor::new(0.0, 0.0, 0.0, 1.0),
        ))
        .expect("rect");
        // No `submit`: the drop at the end of this iteration is what has to
        // finish the pass.
    }

    // The claim is clear, so one more pass can begin — and this one is
    // submitted explicitly.
    let pass = renderer
        .begin_buffer_pass(&buffer, &BufferPassOptions::new())
        .expect("the claim was released");
    pass.submit().expect("submit");
}

/// One live pass per renderer. wlroots' GLES2 renderer treats a pass as a
/// single operation around the global EGL context, and nesting outside a strict
/// stack discipline corrupts it silently rather than failing.
#[test]
fn a_second_pass_on_a_live_renderer_is_refused() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let first_buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");
    let second_buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");

    let pass = renderer
        .begin_buffer_pass(&first_buffer, &BufferPassOptions::new())
        .expect("first pass");

    assert_eq!(
        renderer
            .begin_buffer_pass(&second_buffer, &BufferPassOptions::new())
            .err(),
        Some(Error::Reentrant("Renderer::begin_buffer_pass"))
    );

    pass.submit().expect("submit");

    // Submitting the first releases the claim rather than latching it.
    let pass = renderer
        .begin_buffer_pass(&second_buffer, &BufferPassOptions::new())
        .expect("second pass, once the first is done");
    drop(pass);
}

/// A buffer's producer reference and a texture's own lock are independent: the
/// texture keeps the buffer alive after the producer reference is dropped,
/// which is the refcount rule `crates/wlr/src/buffer.rs` documents.
#[test]
fn a_texture_keeps_its_buffer_alive_after_the_producer_reference_goes() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    let texture = {
        let buffer = allocator.create_buffer(4, 4, &argb()).expect("buffer");
        let mut pass = renderer
            .begin_buffer_pass(&buffer, &BufferPassOptions::new())
            .expect("pass");
        pass.add_rect(&RectOptions::new(
            Box2D::default(),
            RenderColor::new(1.0, 0.0, 0.0, 1.0),
        ))
        .expect("rect");
        pass.submit().expect("submit");
        renderer.texture_from_buffer(&buffer).expect("texture")
    };

    // The `OwnedBuffer` is gone; the texture's own lock is what is left.
    let mut read_back = vec![0u8; 4 * 4 * 4];
    let mut options = wlr::ReadPixels::new(&mut read_back, FourCc::ARGB8888, 4 * 4);
    texture.read_pixels(&mut options).expect("read pixels");
    assert_eq!(&read_back[0..4], &[0x00, 0x00, 0xff, 0xff]);
}

/// Every acquired buffer must be released before the swapchain is destroyed,
/// which the borrow checker enforces — and a swapchain destroyed with all four
/// slots allocated but released must not double free them.
#[test]
fn a_swapchain_can_be_destroyed_after_a_full_cycle() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");

    let swapchain = Swapchain::create(&allocator, 8, 8, &argb()).expect("swapchain");
    for _ in 0..3 {
        let mut held = Vec::new();
        for _ in 0..SWAPCHAIN_CAP {
            held.push(swapchain.acquire().expect("a free slot"));
        }
        assert_eq!(swapchain.in_flight(), SWAPCHAIN_CAP);
        drop(held);
        assert_eq!(swapchain.in_flight(), 0);
    }
    drop(swapchain);
}
