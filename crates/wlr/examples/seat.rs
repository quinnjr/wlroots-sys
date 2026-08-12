//! The `toplevels.rs` example plus a seat: click-to-focus and Escape-to-quit.
//!
//! ```sh
//! WLR_BACKENDS=wayland cargo run -p wlr --example seat
//! # second terminal, using the printed socket:
//! WAYLAND_DISPLAY=wayland-N foot
//! WAYLAND_DISPLAY=wayland-N foot
//! ```
//!
//! Type into the focused window; click the other one and type again — the
//! keystrokes must follow the click. Escape ends the run.

use std::collections::HashMap;

/// `XKB_KEY_Escape`, from `<xkbcommon/xkbcommon-keysyms.h>`. Not pulled in as
/// a dependency-wide constant table for one key; the value is part of the
/// X11 keysym space xkbcommon inherits and has never changed.
const XKB_KEY_ESCAPE: u32 = 0xff1b;

struct App {
    runtime: wlr::Runtime,
    placed: HashMap<wlr::ToplevelId, (i32, i32)>,
    next: (i32, i32),
    quit: bool,
}

impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.quit
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
        // Answering the first commit is what lets the client map at all.
        self.runtime.set_toplevel_size(toplevel.id(), 640, 400);
        self.runtime.set_toplevel_activated(toplevel.id(), true);
    }

    fn mapped(&mut self, toplevel: &wlr::Toplevel<'_>) {
        let id = toplevel.id();
        if let Some(&(x, y)) = self.placed.get(&id) {
            self.runtime.set_toplevel_position(id, x, y);
        }
        self.runtime.raise_toplevel(id);
        // The first mapped window starts out focused, so there is something
        // to type into before the first click ever happens.
        if self.runtime.focus_toplevel_keyboard(id).is_some() {
            self.runtime.set_toplevel_activated(id, true);
        }
        println!("mapped {:?} title={:?}", id, toplevel.title());
    }

    fn title_changed(&mut self, toplevel: &wlr::Toplevel<'_>) {
        println!("title changed {:?} title={:?}", toplevel.id(), toplevel.title());
    }

    fn unmapped(&mut self, id: wlr::ToplevelId) {
        println!("unmapped {id:?}");
    }

    fn toplevel_destroyed(&mut self, id: wlr::ToplevelId) {
        // `remove`, not indexing: this id may be one we were never told about.
        self.placed.remove(&id);
        println!("destroyed {id:?}");
    }
}

impl wlr::SeatHandler for App {
    fn key(&mut self, event: &wlr::KeyEvent<'_>) -> bool {
        if !event.pressed() {
            // Consuming presses but never their matching release would leave
            // a client believing the key is still held; see `SeatHandler::
            // key`'s own doc. Nothing here needs the release, so it is
            // reported but not consumed.
            return false;
        }
        println!("key 0x{:x} mods={:?}", event.keysym(), event.modifiers());
        if event.keysym() == XKB_KEY_ESCAPE {
            self.quit = true;
            return true;
        }
        false
    }

    fn pointer_button(&mut self, x: f64, y: f64, button: u32, pressed: bool, _time_msec: u32) {
        if !pressed {
            return;
        }
        // The smallest complete click-to-focus: find whatever is under the
        // click, put it on top, and give it the keyboard. The click itself
        // (and any subsequent motion) is forwarded to the client
        // unconditionally by the library, independent of this.
        let Some((id, _sx, _sy)) = self.runtime.toplevel_at(x, y) else {
            return;
        };
        self.runtime.raise_toplevel(id);
        self.runtime.set_toplevel_activated(id, true);
        let _ = self.runtime.focus_toplevel_keyboard(id);
        println!("click button=0x{button:x} at ({x:.1}, {y:.1}) -> focus {id:?}");
    }
}

fn main() -> wlr::Result<()> {
    let display = wlr::Display::new()?;
    let runtime = wlr::Runtime::new()?;
    let backend = wlr::Backend::autocreate(&display.event_loop())?;
    runtime.init_graphics(&display, &backend)?;
    runtime.create_xdg_shell(&display, 6)?;
    runtime.create_seat(&display, "seat0")?;

    let bg = runtime.add_rect(4096, 4096, [0.08, 0.09, 0.12, 1.0])?;
    runtime.lower_rect_to_bottom(bg);

    let socket = display.add_socket_auto()?;
    println!("listening on {socket}");

    let mut app = App {
        runtime: runtime.clone(),
        placed: HashMap::new(),
        next: (40, 40),
        quit: false,
    };
    backend.run_all(&display, &mut app, &runtime, wlr::Until::Stop)?;
    Ok(())
}
