//! wlr-layer-shell: every surface a client creates is logged, configured
//! with its own desired size (clamped to a sane minimum), and — once
//! anchored — positioned against the edge(s) it asked for.
//!
//! Run it nested and start a layer-shell client against the socket it
//! prints. `wlr-randr` will not do (it speaks no layer-shell protocol at
//! all); `waybar` is the standard eye test:
//!
//! ```sh
//! WLR_BACKENDS=wayland cargo run -p wlr --example layers
//! # then, in another terminal:
//! WAYLAND_DISPLAY=wayland-N waybar
//! ```

use std::collections::HashMap;

struct App {
    runtime: wlr::Runtime,
    /// The single output's layout box `(x, y, w, h)`, once announced. This
    /// example does not track more than one — a compositor that does would
    /// key this by `OutputId`, resolving each layer surface's own
    /// `output_id()` instead of assuming a single output.
    output_box: Option<(i32, i32, i32, i32)>,
    /// The size this compositor actually chose for each layer surface,
    /// recorded when `configure_layer_surface` is called — `LayerSurface`
    /// exposes only the client's *desired* size, not the negotiated actual
    /// one, so positioning against an edge needs this compositor's own
    /// record of what it configured.
    sizes: HashMap<wlr::LayerSurfaceId, (u32, u32)>,
    /// Whether each layer surface asked for keyboard interactivity, recorded
    /// at `new_layer_surface` time — `layer_surface_mapped` gets only an id,
    /// not a handle, so this is what lets it decide whether to focus.
    wants_keyboard: HashMap<wlr::LayerSurfaceId, bool>,
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
            self.output_box = self.runtime.output_layout_box(output.id());
        }
    }

    fn frame(&mut self, output: &wlr::Output<'_>) {
        let _ = self.runtime.commit_output(output);
    }
}

impl wlr::ToplevelHandler for App {
    fn new_layer_surface(&mut self, surface: &wlr::LayerSurface<'_>) {
        let id = surface.id();
        let desired = surface.desired_size();
        // `0` on either axis means "let the compositor decide"; the client
        // is trusted otherwise. `.max(1)`/`.max(30)` are this example's own
        // arbitrary floor, not a protocol requirement, so a client that
        // never states a size still gets a visible, non-degenerate surface.
        let width = desired.0.max(1);
        let height = desired.1.max(30);
        println!(
            "new layer surface {id:?} layer={:?} anchor={:?} exclusive_zone={} desired={desired:?} \
             keyboard_interactive={} output_id={:?}",
            surface.layer(),
            surface.anchor(),
            surface.exclusive_zone(),
            surface.keyboard_interactive(),
            surface.output_id(),
        );
        self.sizes.insert(id, (width, height));
        self.wants_keyboard
            .insert(id, surface.keyboard_interactive());
        self.runtime.configure_layer_surface(id, width, height);
    }

    fn layer_surface_commit(&mut self, surface: &wlr::LayerSurface<'_>) {
        let id = surface.id();
        let Some((w, h)) = self.sizes.get(&id).copied() else {
            return;
        };
        let Some((ox, oy, ow, oh)) = self.output_box else {
            return;
        };
        let anchor = surface.anchor();

        // Anchored to `left` (whether or not `right` is set too, which
        // means "stretched to fill the axis" and puts the origin at `ox`
        // regardless): flush against the left edge. Anchored to `right`
        // alone: flush against the right edge. Anchored to neither:
        // centered.
        let x = if anchor.left {
            ox
        } else if anchor.right {
            ox + ow - w as i32
        } else {
            ox + (ow - w as i32) / 2
        };
        let y = if anchor.top {
            oy
        } else if anchor.bottom {
            oy + oh - h as i32
        } else {
            oy + (oh - h as i32) / 2
        };

        self.runtime.set_layer_surface_position(id, x, y);
    }

    fn layer_surface_mapped(&mut self, id: wlr::LayerSurfaceId) {
        println!("layer surface mapped {id:?}");
        // Only a surface that actually asked for keyboard interactivity
        // (a launcher, say) gets focused on map — a panel that only shows
        // status text has no use for it, and stealing focus from it would
        // be wrong.
        if self.wants_keyboard.get(&id).copied().unwrap_or(false) {
            self.runtime.focus_layer_keyboard(id);
        }
    }

    fn layer_surface_unmapped(&mut self, id: wlr::LayerSurfaceId) {
        println!("layer surface unmapped {id:?}");
    }

    fn layer_surface_destroyed(&mut self, id: wlr::LayerSurfaceId) {
        self.sizes.remove(&id);
        self.wants_keyboard.remove(&id);
        println!("layer surface destroyed {id:?}");
    }
}

fn main() -> wlr::Result<()> {
    let display = wlr::Display::new()?;
    let runtime = wlr::Runtime::new()?;
    let backend = wlr::Backend::autocreate(&display.event_loop())?;
    runtime.init_graphics(&display, &backend)?;
    runtime.create_xdg_shell(&display, 6)?;
    runtime.create_layer_shell(&display, 4)?;

    let bg = runtime.add_rect(4096, 4096, [0.08, 0.09, 0.12, 1.0])?;
    runtime.lower_rect_to_bottom(bg);

    let socket = display.add_socket_auto()?;
    println!("listening on {socket}");

    let mut app = App {
        runtime: runtime.clone(),
        output_box: None,
        sizes: HashMap::new(),
        wants_keyboard: HashMap::new(),
    };
    backend.run_all(&display, &mut app, &runtime, wlr::Until::Stop)?;
    Ok(())
}
