//! wlr-layer-shell, against a real headless compositor with no client.
//!
//! Same shape as `decoration.rs`'s test file: what is provable without a
//! client library is that the layer-shell global can be created, that the
//! id-keyed mutators reject an id that was never issued rather than
//! dereferencing it, and that the new handler methods are additive.
//!
//! The banded-tree scene tests (band stacking order, reparent-on-layer-
//! change, `raise_toplevel` staying within its band — see `Layer`'s own
//! doc) live in `src/runtime.rs`'s own `#[cfg(test)]` module instead of
//! here: they need to read a live `wlr_scene_tree`'s `children`/`parent`
//! fields directly, which are private to the crate, and an integration test
//! binary like this one only ever sees the crate's public surface — the
//! same reason this file cannot exercise anything client-driven either.

mod common;

#[test]
fn layer_shell_creates_once() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_layer_shell(&display, 4)
        .expect("layer shell");
}

#[test]
fn layer_mutators_on_dead_ids_are_none() {
    common::headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let dead = wlr::LayerSurfaceId::dangling_for_test();
    assert_eq!(runtime.configure_layer_surface(dead, 10, 10), None);
    assert_eq!(runtime.set_layer_surface_position(dead, 0, 0), None);
    assert_eq!(runtime.focus_layer_keyboard(dead), None);
}

#[test]
fn add_rect_in_band_on_a_fresh_runtime_without_graphics_errors() {
    common::headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    // Mirrors add_rect's contract: no graphics yet -> Err, not panic.
    assert!(
        runtime
            .add_rect_in_band(wlr::Band::Overlay, 8, 8, [0.0, 0.0, 0.0, 1.0])
            .is_err()
    );
}

/// `OutputId` has no public dangling constructor (unlike `LayerSurfaceId`
/// and `ToplevelId`; see those types' own `dangling_for_test`), so a live
/// one is captured from a short `run_all`, the same way
/// `output_layout.rs`'s stale-id test does. What is under test here is the
/// *layer-surface* id miss specifically: `set_layer_surface_output`
/// resolves the layer id first (see that method's own doc), so a dead
/// layer id paired with a perfectly live, real output must still be
/// `None` — output resolution is never reached at all.
#[test]
fn set_layer_surface_output_on_dead_ids_is_none() {
    let _serial = common::headless_guard();
    common::headless_env();
    struct App {
        output: Option<wlr::OutputId>,
        runtime: wlr::Runtime,
        turns: u32,
    }
    impl wlr::OutputHandler for App {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            let _ = output.enable_with_preferred_mode();
            let _ = self.runtime.init_output(output);
            self.output = Some(output.id());
        }
    }
    impl wlr::ToplevelHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::FdHandler for App {}
    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            self.turns += 1;
            self.turns > 8 || self.output.is_some()
        }
    }
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    runtime.init_graphics(&display, &backend).expect("graphics");
    let mut app = App {
        output: None,
        runtime: runtime.clone(),
        turns: 0,
    };
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(16))
        .expect("run");
    let output = app
        .output
        .expect("a headless output must have been announced");

    let dead_layer = wlr::LayerSurfaceId::dangling_for_test();
    assert_eq!(runtime.set_layer_surface_output(dead_layer, output), None);
}

#[test]
fn the_layer_methods_are_additive() {
    struct Old;
    impl wlr::OutputHandler for Old {}
    impl wlr::ToplevelHandler for Old {}
    impl wlr::SeatHandler for Old {}
    impl wlr::FdHandler for Old {}
    impl wlr::LoopHandler for Old {}
    fn takes_handlers<S: wlr::Handlers>(_s: &S) {}
    takes_handlers(&Old);
}

/// What the client-driven layer run observed through the new `LayerSurface`
/// operations.
#[derive(Default)]
struct LayerProbe {
    layer_at_new: Option<wlr::Layer>,
    anchor: Option<wlr::Anchor>,
    exclusive_zone: Option<i32>,
    desired_size: Option<(u32, u32)>,
    keyboard_interactive: Option<bool>,
    exclusive_edge_called: bool,
    exclusive_edge: Option<wlr::Edges>,
    popup_surfaces: Option<usize>,
    surface_at_missed: bool,
    popup_surface_at_missed: bool,
    as_layer_surface_resolved: bool,
    destroyed: Option<wlr::LayerSurfaceId>,
    destroy_called: bool,
}

struct LayerApp {
    runtime: wlr::Runtime,
    id: Option<wlr::LayerSurfaceId>,
    probe: LayerProbe,
    client: Option<std::thread::JoinHandle<common::client::ClientEvents>>,
    output: Option<wlr::OutputId>,
}

impl LayerApp {
    fn new(runtime: &wlr::Runtime) -> LayerApp {
        LayerApp {
            runtime: runtime.clone(),
            id: None,
            probe: LayerProbe {
                surface_at_missed: true,
                popup_surface_at_missed: true,
                ..LayerProbe::default()
            },
            client: None,
            output: None,
        }
    }
}

impl wlr::OutputHandler for LayerApp {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        let _ = output.enable_with_preferred_mode();
        let _ = self.runtime.init_output(output);
        self.output = Some(output.id());
    }
}

impl wlr::ToplevelHandler for LayerApp {
    fn new_layer_surface(&mut self, surface: &wlr::LayerSurface<'_>) {
        self.id = Some(surface.id());
        self.probe.layer_at_new = Some(surface.layer());
        // Answering a new layer surface is mandatory and stage-then-flush: the
        // surface is not initialized until its first commit, so this records
        // the size and `on_layer_surface_commit` sends it for real.
        let _ = self.runtime.configure_layer_surface(surface.id(), 64, 48);
    }

    fn layer_surface_commit(&mut self, surface: &wlr::LayerSurface<'_>) {
        self.probe.anchor = Some(surface.anchor());
        self.probe.exclusive_zone = Some(surface.exclusive_zone());
        self.probe.desired_size = Some(surface.desired_size());
        self.probe.keyboard_interactive = Some(surface.keyboard_interactive());
        self.probe.exclusive_edge_called = true;
        self.probe.exclusive_edge = surface.exclusive_edge();
        let mut popups = 0usize;
        surface.for_each_popup_surface(|_, _, _| popups += 1);
        self.probe.popup_surfaces = Some(popups);
        self.probe.surface_at_missed &= surface.surface_at(1.0, 1.0).is_none();
        self.probe.popup_surface_at_missed &= surface.popup_surface_at(1.0, 1.0).is_none();
    }

    fn surface_committed(&mut self, surface: &wlr::Surface<'_>) {
        if surface.as_layer_surface().is_some() {
            self.probe.as_layer_surface_resolved = true;
        }
    }

    fn layer_surface_destroyed(&mut self, id: wlr::LayerSurfaceId) {
        self.probe.destroyed = Some(id);
    }
}

impl wlr::SeatHandler for LayerApp {}
impl wlr::FdHandler for LayerApp {}
impl wlr::LoopHandler for LayerApp {
    fn should_stop(&mut self) -> bool {
        if self.client.as_ref().is_some_and(|h| h.is_finished()) {
            return true;
        }
        // Destroy the layer surface from a turn outside any wlroots callback —
        // the only point wlr_layer_surface_v1_destroy may be called from. The
        // client keeps round-tripping so the server reaches this turn while
        // the surface is alive, and observes the resulting `closed`.
        if !self.probe.destroy_called
            && let Some(id) = self.id
        {
            self.probe.destroy_called = self.runtime.destroy_layer_surface(id).is_some();
        }
        false
    }
}

/// A real layer-shell client states its anchors, exclusive zone, size and
/// keyboard mode, commits, and waits to be closed. The server must observe each
/// through the `LayerSurface` accessors, hit-test empty popup trees without
/// faulting, downcast the generic surface, and destroy the surface through
/// `Runtime::destroy_layer_surface` — which the client sees as `closed`.
#[test]
fn a_real_layer_surface_answers_its_operations_and_destroys() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_layer_shell(&display, 4)
        .expect("layer shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = LayerApp::new(&runtime);
    app.client = Some(common::client::spawn_layer_surface(&socket));

    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Stop)
        .expect("run_all");

    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");
    let probe = &app.probe;

    assert!(
        events.configure_events >= 1 && events.acked_configures >= 1,
        "the server's layer configure must reach the client and be acked"
    );
    assert_eq!(
        probe.layer_at_new,
        Some(wlr::Layer::Top),
        "new_layer_surface sees the request's layer"
    );
    assert_eq!(
        probe.anchor,
        Some(wlr::Anchor {
            top: true,
            ..wlr::Anchor::default()
        }),
        "the committed anchor is read back"
    );
    assert_eq!(
        probe.exclusive_zone,
        Some(32),
        "the committed exclusive zone is read back"
    );
    assert_eq!(
        probe.desired_size,
        Some((64, 48)),
        "the committed desired size is read back"
    );
    assert_eq!(
        probe.keyboard_interactive,
        Some(true),
        "exclusive keyboard interactivity reads as wanting focus"
    );
    assert!(probe.exclusive_edge_called, "exclusive_edge was exercised");
    assert_eq!(
        probe.exclusive_edge,
        Some(wlr::Edges {
            top: true,
            ..wlr::Edges::default()
        }),
        "a top-anchored positive exclusive zone applies to the top edge"
    );
    assert_eq!(
        probe.popup_surfaces,
        Some(0),
        "an empty popup tree iterates nothing"
    );
    assert!(
        probe.surface_at_missed,
        "an unmapped layer surface hit-tests to nothing"
    );
    assert!(
        probe.popup_surface_at_missed,
        "an empty popup tree hit-tests to nothing"
    );
    assert!(
        probe.as_layer_surface_resolved,
        "Surface::as_layer_surface resolves a live layer surface"
    );
    assert!(
        probe.destroy_called,
        "Runtime::destroy_layer_surface destroyed the live layer surface"
    );
    assert!(
        probe.destroyed == app.id,
        "the layer_surface_destroyed event names the destroyed surface"
    );
    assert!(
        events.layer_closed,
        "destroying the layer surface sends the client a closed event"
    );
}

/// `Runtime::destroy_layer_surface` on an id nothing issued is a clean miss.
#[test]
fn destroy_layer_surface_on_a_dead_id_is_none() {
    common::headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    assert_eq!(
        runtime.destroy_layer_surface(wlr::LayerSurfaceId::dangling_for_test()),
        None
    );
}
