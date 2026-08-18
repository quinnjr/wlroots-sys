//! Draw a rectangle and a textured quad into an allocator buffer, read the
//! pixels back, and print a checksum.
//!
//! No outputs, no event loop, no client: this is the render pipeline on its
//! own — renderer, allocator, buffer, pass, texture — which is what makes it
//! the shortest complete example of the render API in this crate.
//!
//! ```sh
//! cargo run -p wlr --example render_pass
//! # or, to make it pick the GPU renderer instead:
//! WLR_RENDERER=gles2 cargo run -p wlr --example render_pass
//! ```
//!
//! Both `WLR_BACKENDS` and `WLR_RENDERER` default to `headless` and `pixman`
//! here if the environment does not set them, so the example runs anywhere.
//! That is not timidity: a *render target* has to use a format and modifier the
//! renderer can draw into, and which those are comes from the output or the
//! backend rather than from anything this example has. Asking a GPU renderer to
//! draw into a hand-picked `ARGB8888`/`LINEAR` buffer from the generic
//! allocator gets "DMA-BUF format is external-only" on some drivers — a real
//! answer to a question this example is not the right place to ask. Wiring a
//! swapchain to an output's own format is what a compositor does, and what M6's
//! output work will show.

use wlr::{
    Allocator, Backend, Box2D, BufferPassOptions, Display, DrmFormat, FourCc, Modifier, ReadPixels,
    RectOptions, Region, RenderColor, Renderer, TextureOptions,
};

const WIDTH: i32 = 64;
const HEIGHT: i32 = 64;

fn main() -> wlr::Result<()> {
    // SAFETY: single-threaded, before anything else in the process runs.
    unsafe {
        if std::env::var_os("WLR_BACKENDS").is_none() {
            std::env::set_var("WLR_BACKENDS", "headless");
        }
        if std::env::var_os("WLR_RENDERER").is_none() {
            std::env::set_var("WLR_RENDERER", "pixman");
        }
    }

    let display = Display::new()?;
    let backend = Backend::autocreate(&display.event_loop())?;

    // A renderer this example owns: dropping it at the end of `main` destroys
    // it, and everything below borrows it so nothing can outlive it.
    let renderer = Renderer::autocreate(&backend)?;
    println!(
        "renderer: buffer caps {:#x}, features {:?}",
        renderer.buffer_caps().bits(),
        renderer.features()
    );

    let allocator = Allocator::autocreate(&backend, &renderer)?;
    let format = DrmFormat::new(FourCc::ARGB8888, [Modifier::LINEAR]);
    let buffer = allocator.create_buffer(WIDTH, HEIGHT, &format)?;

    // A 8×8 checkerboard to draw with. `texture_from_pixels` copies, so this
    // `Vec` can go out of scope whenever it likes.
    let texture = {
        let mut pixels = Vec::with_capacity(8 * 8 * 4);
        for y in 0..8 {
            for x in 0..8 {
                let lit = (x + y) % 2 == 0;
                // ARGB8888 is stored little-endian, so the bytes are B, G, R, A.
                pixels.extend_from_slice(if lit {
                    &[0x00, 0xff, 0x00, 0xff]
                } else {
                    &[0x00, 0x40, 0x00, 0xff]
                });
            }
        }
        renderer.texture_from_pixels(FourCc::ARGB8888, 8 * 4, 8, 8, &pixels)?
    };

    {
        let mut pass = renderer.begin_buffer_pass(&buffer, &BufferPassOptions::new())?;

        // A blue background over the whole buffer — an empty box means exactly
        // that.
        pass.add_rect(&RectOptions::new(
            Box2D::default(),
            RenderColor::new(0.0, 0.0, 0.5, 1.0),
        ))?;

        // The checkerboard, scaled up into the middle, clipped to a band so the
        // clip parameter is exercised too.
        let clip = Region::from_box(Box2D::new(0, 8, WIDTH, HEIGHT - 16));
        pass.add_texture(
            &TextureOptions::new(&texture)
                .dst_box(Box2D::new(16, 16, 32, 32))
                .clip(&clip),
        )?;

        // No `submit()`: dropping the pass submits it, which is the one thing
        // wlroots offers. `submit()` is the same call with the answer returned.
    }

    // Read the result back through a texture over the same buffer.
    let readback = renderer.texture_from_buffer(&buffer)?;
    let stride = (WIDTH as u32) * 4;
    let mut pixels = vec![0u8; stride as usize * HEIGHT as usize];
    let mut options = ReadPixels::new(&mut pixels, FourCc::ARGB8888, stride);
    match readback.read_pixels(&mut options) {
        Ok(()) => {
            // A checksum rather than a dump: this is a smoke test with a value
            // a human can compare between runs.
            let sum: u64 = pixels.iter().map(|b| u64::from(*b)).sum();
            let nonzero = pixels.iter().filter(|b| **b != 0).count();
            println!(
                "read back {} bytes, checksum {sum}, {nonzero} non-zero",
                pixels.len()
            );
        }
        Err(err) => {
            println!("this renderer refused the read-back ({err}); the pass still ran");
        }
    }

    Ok(())
}
