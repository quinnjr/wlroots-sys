//! Logs every client state/move/resize request, and honours maximize by
//! staging the output's own size on the toplevel.
//!
//! Run it nested and try a CSD client's maximize button:
//!
//! ```sh
//! WLR_BACKENDS=wayland cargo run -p wlr --example client_requests
//! # then, in another terminal:
//! WAYLAND_DISPLAY=wayland-N foot
//! ```

struct App {
    runtime: wlr::Runtime,
    /// The most recently announced output's size, in this compositor's own
    /// (layout) coordinates. `(0, 0)` until at least one output has been
    /// enabled — a single-output nested setup, which is all this example
    /// targets, never needs more than the one value.
    output_size: (i32, i32),
}

impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        false
    }
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        if output.enable_with_preferred_mode().is_err() {
            return;
        }
        if self.runtime.init_output(output).is_ok() {
            self.output_size = output.size();
        }
    }

    fn frame(&mut self, output: &wlr::Output<'_>) {
        let _ = self.runtime.commit_output(output);
    }
}

impl wlr::ToplevelHandler for App {
    fn initial_commit(&mut self, toplevel: &wlr::Toplevel<'_>) {
        self.runtime.set_toplevel_size(toplevel.id(), 640, 400);
        self.runtime.set_toplevel_activated(toplevel.id(), true);
    }

    fn request_maximize(&mut self, toplevel: &wlr::Toplevel<'_>, maximize: bool) {
        let id = toplevel.id();
        println!("request_maximize {id:?} -> {maximize}");

        // Honour it: stage the maximized flag and, when maximizing, the
        // output's own size. Un-maximizing is left at whatever size the
        // client last had — there is no "restore size" tracked by this
        // minimal example. Staging here is exactly what makes the
        // dispatch-layer configure that follows this call carry the state
        // this handler asked for, rather than a bare one: wlroots coalesces
        // the two into a single configure.
        self.runtime.set_toplevel_maximized(id, maximize);
        if maximize {
            let (w, h) = self.output_size;
            if w > 0 && h > 0 {
                self.runtime.set_toplevel_size(id, w, h);
                self.runtime.set_toplevel_position(id, 0, 0);
            }
        }
    }

    fn request_fullscreen(&mut self, toplevel: &wlr::Toplevel<'_>, fullscreen: bool) {
        println!(
            "request_fullscreen {:?} -> {fullscreen} (logged only, not honoured)",
            toplevel.id()
        );
    }

    fn request_move(&mut self, id: wlr::ToplevelId) {
        println!("request_move {id:?} (logged only, not honoured)");
    }

    fn request_resize(&mut self, id: wlr::ToplevelId, edges: wlr::Edges) {
        println!("request_resize {id:?} edges={edges:?} (logged only, not honoured)");
    }
}

fn main() -> wlr::Result<()> {
    let display = wlr::Display::new()?;
    let runtime = wlr::Runtime::new()?;
    let backend = wlr::Backend::autocreate(&display.event_loop())?;
    runtime.init_graphics(&display, &backend)?;
    runtime.create_xdg_shell(&display, 6)?;

    let bg = runtime.add_rect(4096, 4096, [0.08, 0.09, 0.12, 1.0])?;
    runtime.lower_rect_to_bottom(bg);

    let socket = display.add_socket_auto()?;
    println!("listening on {socket}");

    let mut app = App {
        runtime: runtime.clone(),
        output_size: (0, 0),
    };
    backend.run_all(&display, &mut app, &runtime, wlr::Until::Stop)?;
    Ok(())
}
