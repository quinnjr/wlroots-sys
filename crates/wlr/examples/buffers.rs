//! Bring up whatever backend the environment offers, paint a background, and
//! add an 8x8 red/blue checkerboard pixel-buffer node on top of it, scaled
//! up to 256x256 on screen.
//!
//! ```sh
//! WLR_BACKENDS=headless cargo run -p wlr --example buffers
//! ```

struct App {
    runtime: wlr::Runtime,
    frames: u32,
}

impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::SessionLockHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.frames >= 60
    }
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        // No `unwrap` anywhere in a handler body: this frame is `extern "C"`.
        if output.enable_with_preferred_mode().is_err() {
            return;
        }
        if self.runtime.init_output(output).is_err() {
            return;
        }
        println!("output {:?} up at {:?}", output.id(), output.size());
    }

    fn frame(&mut self, output: &wlr::Output<'_>) {
        let _ = self.runtime.commit_output(output);
        self.frames += 1;
    }
}

/// An 8x8 checkerboard, alternating opaque red and opaque blue one pixel at
/// a time, in the R, G, B, A byte order `Runtime::add_buffer` documents.
fn checkerboard(size: usize) -> Vec<u8> {
    let mut px = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let i = (y * size + x) * 4;
            if (x + y) % 2 == 0 {
                px[i..i + 4].copy_from_slice(&[255, 0, 0, 255]); // red
            } else {
                px[i..i + 4].copy_from_slice(&[0, 0, 255, 255]); // blue
            }
        }
    }
    px
}

fn main() -> wlr::Result<()> {
    let display = wlr::Display::new()?;
    let backend = wlr::Backend::autocreate(&display.event_loop())?;
    let runtime = wlr::Runtime::new()?;
    runtime.init_graphics(&display, &backend)?;

    let bg = runtime.add_rect(4096, 4096, [0.08, 0.09, 0.12, 1.0])?;
    let _ = runtime.lower_rect_to_bottom(bg);

    let px = checkerboard(8);
    let buffer = runtime.add_buffer(8, 8, &px)?;
    runtime
        .set_buffer_position(buffer, 32, 32)
        .expect("buffer was just created");
    runtime
        .set_buffer_dest_size(buffer, 256, 256)
        .expect("buffer was just created");

    let socket = display.add_socket_auto()?;
    println!("listening on {socket}");

    let mut app = App {
        runtime: runtime.clone(),
        frames: 0,
    };
    backend.run_all(&display, &mut app, &runtime, wlr::Until::Turns(60))?;
    println!("drew {} frames", app.frames);
    Ok(())
}
