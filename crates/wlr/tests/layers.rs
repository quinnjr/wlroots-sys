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
    /// How many surfaces the last `for_each_surface` walk yielded, and
    /// whether every yielded id resolved through `Runtime::surface`.
    walk_surfaces: Option<usize>,
    walk_all_resolved: bool,
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
        // The full-tree walk: count every yielded surface and resolve each
        // id through `Runtime::surface`, the layer-shell half of the
        // role-id-versus-surface-id regression `xdg_remainder.rs` covers for
        // toplevels.
        let mut walked = 0usize;
        let mut all_resolved = true;
        surface.for_each_surface(|leaf, _, _| {
            walked += 1;
            if self.runtime.surface(leaf.id()).is_none() {
                all_resolved = false;
            }
        });
        self.probe.walk_surfaces = Some(walked);
        self.probe.walk_all_resolved = all_resolved;
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

/// Shared headless bootstrap for the destroying layer runs below: display,
/// backend, graphics, layer shell, one client, run to the client's
/// disconnect, join.
fn run_layer_app(
    spawn: impl FnOnce(&str) -> std::thread::JoinHandle<common::client::ClientEvents>,
) -> (LayerApp, common::client::ClientEvents) {
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
    app.client = Some(spawn(&socket));

    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Stop)
        .expect("run_all");

    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");
    (app, events)
}

/// A real layer-shell client states its anchors, exclusive zone, size and
/// keyboard mode, commits, and waits to be closed. The server must observe each
/// through the `LayerSurface` accessors, hit-test empty popup trees without
/// faulting, downcast the generic surface, and destroy the surface through
/// `Runtime::destroy_layer_surface` — which the client sees as `closed`.
#[test]
fn a_real_layer_surface_answers_its_operations_and_destroys() {
    let (app, events) = run_layer_app(common::client::spawn_layer_surface);
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
        probe.walk_surfaces.is_some_and(|n| n >= 1),
        "for_each_surface yields at least the root, got {:?}",
        probe.walk_surfaces
    );
    assert!(
        probe.walk_all_resolved,
        "every yielded SurfaceId resolves through Runtime::surface"
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

/// What the mapped layer runs observed: walk yields and hit-test outcomes.
///
/// A separate app from [`LayerApp`] because these runs must never destroy:
/// `LayerApp::should_stop` destroys on the first turn that knows an id,
/// which can land before the client's buffered (mapping) commit is even
/// dispatched — and then nothing is ever mapped and every hit assertion
/// below fails. This app only watches; the run ends when the client
/// disconnects.
#[derive(Default)]
struct MappedProbe {
    /// Largest `for_each_surface` yield seen across commits, and whether
    /// every id in that walk resolved through `Runtime::surface`.
    walk_max: usize,
    walk_max_resolved: bool,
    /// Whether `surface_at` inside the surface hit a leaf that resolved.
    /// `None` until the first hit, so the bufferless first commit (a miss)
    /// cannot clear a later mapped hit.
    hit_resolved: Option<bool>,
    /// `popup_surface_at` inside the surface misses on every commit: a layer
    /// surface hosts no popup tree. Starts true, like
    /// [`LayerProbe::surface_at_missed`](LayerProbe::surface_at_missed).
    popup_inside_missed: bool,
    /// Both hit-tests miss far outside the tree on every commit.
    far_missed: bool,
}

struct MappedLayerApp {
    runtime: wlr::Runtime,
    id: Option<wlr::LayerSurfaceId>,
    probe: MappedProbe,
    client: Option<std::thread::JoinHandle<common::client::ClientEvents>>,
}

impl MappedLayerApp {
    fn new(runtime: &wlr::Runtime) -> MappedLayerApp {
        MappedLayerApp {
            runtime: runtime.clone(),
            id: None,
            probe: MappedProbe {
                popup_inside_missed: true,
                far_missed: true,
                ..MappedProbe::default()
            },
            client: None,
        }
    }
}

impl wlr::OutputHandler for MappedLayerApp {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        let _ = output.enable_with_preferred_mode();
        let _ = self.runtime.init_output(output);
    }
}

impl wlr::ToplevelHandler for MappedLayerApp {
    fn new_layer_surface(&mut self, surface: &wlr::LayerSurface<'_>) {
        self.id = Some(surface.id());
        // As in `LayerApp`: answering is mandatory, and the size the mapped
        // clients attach (64x48) is what is configured here.
        let _ = self.runtime.configure_layer_surface(surface.id(), 64, 48);
    }

    fn layer_surface_commit(&mut self, surface: &wlr::LayerSurface<'_>) {
        let mut count = 0usize;
        let mut all_resolved = true;
        surface.for_each_surface(|leaf, _, _| {
            count += 1;
            if self.runtime.surface(leaf.id()).is_none() {
                all_resolved = false;
            }
        });
        if count >= self.probe.walk_max {
            self.probe.walk_max = count;
            self.probe.walk_max_resolved = all_resolved;
        }
        if let Some((leaf, _, _)) = surface.surface_at(1.0, 1.0) {
            self.probe.hit_resolved = Some(self.runtime.surface(leaf.id()).is_some());
        }
        self.probe.popup_inside_missed &= surface.popup_surface_at(1.0, 1.0).is_none();
        self.probe.far_missed &= surface.surface_at(1_000_000.0, 1_000_000.0).is_none();
        self.probe.far_missed &= surface.popup_surface_at(1_000_000.0, 1_000_000.0).is_none();
    }
}

impl wlr::SeatHandler for MappedLayerApp {}
impl wlr::FdHandler for MappedLayerApp {}
impl wlr::LoopHandler for MappedLayerApp {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

/// As [`run_layer_app`], but for the watching [`MappedLayerApp`]: same
/// bootstrap, no destroy.
fn run_mapped_layer_app(
    spawn: impl FnOnce(&str) -> std::thread::JoinHandle<common::client::ClientEvents>,
) -> (MappedLayerApp, common::client::ClientEvents) {
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

    let mut app = MappedLayerApp::new(&runtime);
    app.client = Some(spawn(&socket));

    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Stop)
        .expect("run_all");

    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");
    (app, events)
}

/// A mapped layer surface hit-tests: `surface_at` inside strikes a leaf that
/// resolves through `Runtime::surface`, and both hit-tests miss far outside.
/// `popup_surface_at` inside still misses — a layer surface hosts no popup
/// tree, so there is no popup leaf to strike; that half of the brief cannot
/// be a `Some` and is pinned as a miss instead.
#[test]
fn a_mapped_layer_surface_hits_inside_and_misses_outside() {
    let (app, events) = run_mapped_layer_app(common::client::spawn_layer_surface_mapped);
    let probe = &app.probe;

    assert!(
        events.configure_events >= 1 && events.acked_configures >= 1,
        "the server's layer configure must reach the client and be acked"
    );
    assert!(app.id.is_some(), "a layer surface was announced");
    assert_eq!(
        probe.hit_resolved,
        Some(true),
        "surface_at inside the mapped surface hits a resolvable leaf"
    );
    assert!(
        probe.popup_inside_missed,
        "a layer surface hosts no popup tree, so popup_surface_at inside misses"
    );
    assert!(probe.far_missed, "both hit-tests miss far outside the tree");
    assert!(
        probe.walk_max >= 1 && probe.walk_max_resolved,
        "for_each_surface yields at least the root, all resolvable"
    );
}

/// A mapped sub-surface child on a layer surface is visited by the
/// full-tree walk with a resolvable id: the largest walk names both.
#[test]
fn a_mapped_subsurface_is_walked_on_a_layer_surface() {
    let (app, events) =
        run_mapped_layer_app(common::client::spawn_layer_surface_mapped_with_subsurface);
    let probe = &app.probe;

    assert!(
        events.configure_events >= 1 && events.acked_configures >= 1,
        "the server's layer configure must reach the client and be acked"
    );
    assert!(
        probe.walk_max >= 2,
        "the walk visited the root and the mapped sub-surface, got {}",
        probe.walk_max
    );
    assert!(
        probe.walk_max_resolved,
        "every yielded SurfaceId resolved through Runtime::surface"
    );
}

/// A nonpositive exclusive zone applies to no edge: the same headless layer
/// run with `set_exclusive_zone(0)` reads `exclusive_edge() == None`,
/// alongside the existing run's `Some(top)` for a positive zone.
#[test]
fn exclusive_edge_is_none_for_a_nonpositive_zone() {
    let (app, _events) =
        run_layer_app(|socket| common::client::spawn_layer_surface_with_zone(socket, 0));
    let probe = &app.probe;

    assert_eq!(
        probe.exclusive_zone,
        Some(0),
        "the committed zero zone is read back"
    );
    assert!(probe.exclusive_edge_called, "exclusive_edge was exercised");
    assert_eq!(
        probe.exclusive_edge, None,
        "a zero exclusive zone applies to no edge"
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
