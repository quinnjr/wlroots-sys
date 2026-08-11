//! Bring up whatever backend the environment offers, paint a background, and
//! commit the scene on every frame.
//!
//! ```sh
//! WLR_BACKENDS=headless cargo run -p wlr --example scene_background
//! # or, nested inside an existing Wayland session:
//! WLR_BACKENDS=wayland cargo run -p wlr --example scene_background
//! ```

struct App {
    runtime: wlr::Runtime,
    frames: u32,
}

impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.frames >= 120
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

fn main() -> wlr::Result<()> {
    let display = wlr::Display::new()?;
    let backend = wlr::Backend::autocreate(&display.event_loop())?;
    let runtime = wlr::Runtime::new()?;
    runtime.init_graphics(&display, &backend)?;

    let bg = runtime.add_rect(4096, 4096, [0.08, 0.09, 0.12, 1.0])?;
    let _ = runtime.lower_rect_to_bottom(bg);

    let socket = display.add_socket_auto()?;
    println!("listening on {socket}");

    let mut app = App { runtime: runtime.clone(), frames: 0 };
    backend.run_all(&display, &mut app, &runtime, wlr::Until::Stop)?;
    println!("drew {} frames", app.frames);
    Ok(())
}
