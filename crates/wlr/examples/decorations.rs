//! `toplevels.rs`, plus the xdg-decoration manager: every client is forced
//! into server-side decorations, whatever it asks for.
//!
//! Run it nested and start a client against the socket it prints:
//!
//! ```sh
//! WLR_BACKENDS=wayland cargo run -p wlr --example decorations
//! # then, in another terminal:
//! WAYLAND_DISPLAY=wayland-N foot
//! ```

use std::collections::HashMap;

struct App {
    runtime: wlr::Runtime,
    placed: HashMap<wlr::ToplevelId, (i32, i32)>,
    next: (i32, i32),
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
        let _ = self.runtime.init_output(output);
    }

    fn frame(&mut self, output: &wlr::Output<'_>) {
        let _ = self.runtime.commit_output(output);
    }
}

impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, toplevel: &wlr::Toplevel<'_>) {
        let at = self.next;
        self.next = (at.0 + 32, at.1 + 32);
        self.placed.insert(toplevel.id(), at);
        println!(
            "new toplevel {:?} app_id={:?} pid={:?}",
            toplevel.id(),
            toplevel.app_id(),
            toplevel.pid()
        );
    }

    fn initial_commit(&mut self, toplevel: &wlr::Toplevel<'_>) {
        self.runtime.set_toplevel_size(toplevel.id(), 640, 400);
        self.runtime.set_toplevel_activated(toplevel.id(), true);
    }

    fn mapped(&mut self, toplevel: &wlr::Toplevel<'_>) {
        let id = toplevel.id();
        if let Some(&(x, y)) = self.placed.get(&id) {
            self.runtime.set_toplevel_position(id, x, y);
        }
        self.runtime.raise_toplevel(id);
        println!("mapped {:?} title={:?}", id, toplevel.title());
    }

    fn title_changed(&mut self, toplevel: &wlr::Toplevel<'_>) {
        println!(
            "title changed {:?} title={:?}",
            toplevel.id(),
            toplevel.title()
        );
    }

    fn unmapped(&mut self, id: wlr::ToplevelId) {
        println!("unmapped {id:?}");
    }

    fn toplevel_destroyed(&mut self, id: wlr::ToplevelId) {
        self.placed.remove(&id);
        println!("destroyed {id:?}");
    }

    fn request_decoration_mode(
        &mut self,
        id: wlr::ToplevelId,
        preference: Option<wlr::DecorationMode>,
    ) {
        // Logged, then overridden: whatever the client asked for, this
        // compositor draws its own decorations. Calling
        // `set_decoration_mode` here is what stops the dispatch layer's
        // server-side default from firing a second time once this method
        // returns — it would be redundant in this particular example (the
        // default is server-side too), but a real compositor needs exactly
        // this call. One that wanted to *honour* the client instead would
        // write `set_decoration_mode(id, preference.unwrap_or(ServerSide))`:
        // the same type comes in and goes out, so there is no polarity to
        // get backwards.
        println!(
            "decoration mode requested for {id:?}: preference={preference:?}, forcing server-side"
        );
        self.runtime
            .set_decoration_mode(id, wlr::DecorationMode::ServerSide);
    }
}

fn main() -> wlr::Result<()> {
    let display = wlr::Display::new()?;
    let runtime = wlr::Runtime::new()?;
    let backend = wlr::Backend::autocreate(&display.event_loop())?;
    runtime.init_graphics(&display, &backend)?;
    runtime.create_xdg_shell(&display, 6)?;
    runtime.create_xdg_decoration_manager(&display)?;

    let bg = runtime.add_rect(4096, 4096, [0.08, 0.09, 0.12, 1.0])?;
    runtime.lower_rect_to_bottom(bg);

    let socket = display.add_socket_auto()?;
    println!("listening on {socket}");

    let mut app = App {
        runtime: runtime.clone(),
        placed: HashMap::new(),
        next: (40, 40),
    };
    backend.run_all(&display, &mut app, &runtime, wlr::Until::Stop)?;
    Ok(())
}
