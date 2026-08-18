//! Build a small scene tree, restack it, reparent it, and print its shape.
//!
//! ```sh
//! WLR_BACKENDS=headless cargo run -p wlr --example scene_tree
//! # or, nested inside an existing Wayland session:
//! WLR_BACKENDS=wayland cargo run -p wlr --example scene_tree
//! ```
//!
//! Everything interesting here happens before the event loop starts: a scene is
//! fully usable the moment `init_graphics` returns, and every node operation is
//! a by-id call on `Runtime` that misses cleanly rather than panicking. The run
//! is only there to put the result on a screen.

/// Print `node` and its subtree, one line per node, indented by depth.
///
/// Uses only by-id queries, so a node that dies between two lines shows up as
/// a missing subtree rather than as a crash.
fn dump(runtime: &wlr::Runtime, node: wlr::NodeId, depth: usize, label: &str) {
    let indent = "  ".repeat(depth);
    let kind = runtime.node_kind(node);
    let position = runtime.node_position(node).unwrap_or((0, 0));
    let coords = runtime.node_coords(node);
    println!(
        "{indent}{label} {node:?} {kind:?} at {position:?} \
         (layout {coords:?})"
    );
    for (i, child) in runtime
        .node_children(node)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        dump(runtime, child, depth + 1, &format!("#{i}"));
    }
}

struct App {
    runtime: wlr::Runtime,
    panel: wlr::NodeId,
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
        // Slide the panel across, so the scene is actually doing something.
        let x = (self.frames as i32) % 200;
        let _ = self.runtime.set_node_position(self.panel, x, 0);
        let _ = self.runtime.commit_output(output);
        self.frames += 1;
    }
}

fn main() -> wlr::Result<()> {
    let display = wlr::Display::new()?;
    let backend = wlr::Backend::autocreate(&display.event_loop())?;
    let runtime = wlr::Runtime::new()?;
    runtime.init_graphics(&display, &backend)?;

    let root = runtime.scene_root_node().expect("init_graphics ran");

    // A background rect parented into the *bottom* band, so it stacks with the
    // band rather than floating above every one of them — see `Runtime::add_rect`
    // for the trap that avoids.
    let background_band = runtime.band_node(wlr::Band::Background).expect("band");
    let background = runtime
        .create_rect(background_band, 4096, 4096, [0.08, 0.09, 0.12, 1.0])
        .expect("background");

    // A "panel": a tree with two rects in it, so reparenting moves a subtree
    // rather than a single node.
    let panel = runtime
        .create_tree_in_band(wlr::Band::Toplevel)
        .expect("panel");
    let bar = runtime
        .create_rect(panel, 200, 24, [0.15, 0.16, 0.22, 1.0])
        .expect("bar");
    let dot = runtime
        .create_rect(panel, 12, 12, [0.9, 0.4, 0.2, 1.0])
        .expect("dot");
    runtime.set_node_position(dot, 6, 6).expect("place the dot");

    println!("--- as built ---");
    dump(&runtime, root, 0, "root");

    // Restacking only ever reorders siblings, so this puts the dot over the bar
    // and cannot lift either out of the panel.
    runtime.place_node_above(bar, dot).expect("bar over dot");
    println!(
        "bar is now above the dot: {:?}",
        runtime.node_children(panel)
    );
    runtime.lower_node_to_bottom(bar).expect("bar back down");

    // Reparenting is the operation that *can* cross bands.
    let overlay = runtime.band_node(wlr::Band::Overlay).expect("band");
    runtime
        .reparent_node(panel, overlay)
        .expect("panel to the overlay band");
    println!(
        "panel moved to the overlay band; its parent is now {:?}",
        runtime.node_parent(panel)
    );

    // Handles are read-only views that cannot escape the closure they are
    // handed to — the storable identity is the id.
    runtime
        .with_rect(background, |rect| {
            println!("background is {:?} in {:?}", rect.size(), rect.color());
        })
        .expect("background is a rect");

    println!("--- after restack and reparent ---");
    dump(&runtime, root, 0, "root");

    let socket = display.add_socket_auto()?;
    println!("listening on {socket}");

    let mut app = App {
        runtime: runtime.clone(),
        panel,
        frames: 0,
    };
    backend.run_all(&display, &mut app, &runtime, wlr::Until::Stop)?;

    // Destroying the panel takes both rects with it: wlroots frees a tree's
    // descendants recursively, and every one of their ids goes stale at once.
    runtime.destroy_node(panel).expect("panel destroyed");
    println!(
        "after destroying the panel: bar={:?} dot={:?}",
        runtime.node_kind(bar),
        runtime.node_kind(dot)
    );

    println!("drew {} frames", app.frames);
    Ok(())
}
