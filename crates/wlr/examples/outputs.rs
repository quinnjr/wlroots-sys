//! Bring up a two-output headless backend and print each output's name and
//! layout box as it is placed.
//!
//! ```sh
//! cargo run -p wlr --example outputs
//! ```
//!
//! Sets `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` itself, in `main`, before
//! `Backend::autocreate` — examples process their own env rather than
//! relying on the caller's shell, the same rule `crates/wlr/tests/*.rs`
//! follow for the same reason: `WLR_BACKENDS` must be set before wlroots
//! picks a backend, and nothing later can change its mind.

struct App {
    runtime: wlr::Runtime,
    seen: u32,
}

impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.seen >= 2
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
        self.seen += 1;
        let name = output.name().unwrap_or_default();
        match self.runtime.output_layout_box(output.id()) {
            Some((x, y, width, height)) => {
                println!("{name}: box ({x}, {y}, {width}, {height})");
            }
            None => println!("{name}: not in the layout"),
        }
    }
}

fn main() -> wlr::Result<()> {
    // SAFETY: single-threaded at this point in `main`, before any other code
    // in the process reads or writes the environment.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "2");
    }

    let display = wlr::Display::new()?;
    let backend = wlr::Backend::autocreate(&display.event_loop())?;
    let runtime = wlr::Runtime::new()?;
    runtime.init_graphics(&display, &backend)?;

    let mut app = App {
        runtime: runtime.clone(),
        seen: 0,
    };
    backend.run_all(&display, &mut app, &runtime, wlr::Until::Turns(16))?;
    println!("saw {} output(s)", app.seen);
    Ok(())
}
