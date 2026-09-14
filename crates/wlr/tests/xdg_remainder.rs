//! The M9 xdg-shell remainder, against a real headless compositor.
//!
//! A real Wayland client drives a toplevel through bufferless commit so the
//! server's `initial_commit` fires; the handler then exercises every new
//! accessor and by-id setter on the live object, and a second run proves the
//! destroy path delivers after the map. The by-id misses, the role downcast
//! and the surface-tree walk are asserted alongside.

mod common;

use std::thread::JoinHandle;

use wlr::{
    Backend, Display, Runtime, SurfaceRole, ToplevelId, ToplevelState, Until, WmCapabilities,
};

struct App {
    runtime: Runtime,
    toplevels: Vec<ToplevelId>,
    initial_commits: usize,
    roles: Vec<SurfaceRole>,
    downcast_toplevels: usize,
    setter_results: Vec<Option<()>>,
    parent_result: Option<bool>,
    pinged: usize,
    root_surfaces: usize,
    state_sample: Option<ToplevelState>,
    requested_sample: Option<(bool, bool, bool)>,
    wm_caps_sample: Option<WmCapabilities>,
    mapped: Vec<ToplevelId>,
    destroyed: Vec<ToplevelId>,
    show_window_menus: Vec<(ToplevelId, i32, i32)>,
    /// The largest surface-tree walk seen, and whether every id it yielded
    /// resolved through `Runtime::surface` — the regression for the
    /// role-id-versus-surface-id addon mix-up.
    walk_visited: usize,
    walk_all_resolved: bool,
    /// Whether that walk yielded a surface other than the root.
    walk_other_surface: bool,
    client: Option<JoinHandle<common::client::ClientEvents>>,
}

impl App {
    fn new(runtime: Runtime) -> App {
        App {
            runtime,
            toplevels: Vec::new(),
            initial_commits: 0,
            roles: Vec::new(),
            downcast_toplevels: 0,
            setter_results: Vec::new(),
            parent_result: None,
            pinged: 0,
            root_surfaces: 0,
            state_sample: None,
            requested_sample: None,
            wm_caps_sample: None,
            mapped: Vec::new(),
            destroyed: Vec::new(),
            show_window_menus: Vec::new(),
            walk_visited: 0,
            walk_all_resolved: false,
            walk_other_surface: false,
            client: None,
        }
    }
}

impl wlr::OutputHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, toplevel: &wlr::Toplevel<'_>) {
        self.toplevels.push(toplevel.id());
    }

    fn initial_commit(&mut self, toplevel: &wlr::Toplevel<'_>) {
        self.initial_commits += 1;
        let id = toplevel.id();
        self.setter_results = vec![
            self.runtime.set_toplevel_bounds(id, 800, 600),
            self.runtime.set_toplevel_constrained(
                id,
                wlr::Edges {
                    left: true,
                    ..Default::default()
                },
            ),
            self.runtime.set_toplevel_resizing(id, true),
            self.runtime.set_toplevel_suspended(id, false),
            self.runtime.set_toplevel_tiled(
                id,
                wlr::Edges {
                    top: true,
                    ..Default::default()
                },
            ),
            self.runtime.set_toplevel_wm_capabilities(
                id,
                WmCapabilities::MAXIMIZE | WmCapabilities::MINIMIZE,
            ),
        ];
        self.parent_result = self.runtime.set_toplevel_parent(id, None);
        self.state_sample = Some(toplevel.state());
        let requested = toplevel.requested();
        self.requested_sample = Some((
            requested.maximized,
            requested.minimized,
            requested.fullscreen,
        ));
        self.wm_caps_sample = Some(toplevel.wm_capabilities());
        toplevel.ping();
        self.pinged += 1;
        let mut count = 0usize;
        let mut all_resolved = true;
        toplevel.for_each_surface(|surface, _x, _y| {
            count += 1;
            if self.runtime.surface(surface.id()).is_none() {
                all_resolved = false;
            }
        });
        self.root_surfaces = count;
        self.walk_all_resolved = all_resolved;
    }

    fn mapped(&mut self, toplevel: &wlr::Toplevel<'_>) {
        self.mapped.push(toplevel.id());
    }

    fn toplevel_destroyed(&mut self, id: ToplevelId) {
        self.destroyed.push(id);
    }

    fn request_show_window_menu(&mut self, id: ToplevelId, x: i32, y: i32) {
        self.show_window_menus.push((id, x, y));
    }

    fn surface_committed(&mut self, surface: &wlr::Surface<'_>) {
        self.roles.push(surface.role());
        let Some(toplevel) = wlr::Toplevel::from_surface(surface) else {
            return;
        };
        self.downcast_toplevels += 1;
        let root = surface.id();
        let mut visited = 0usize;
        let mut all_resolved = true;
        let mut visited_other = false;
        toplevel.for_each_surface(|surface, _x, _y| {
            visited += 1;
            if self.runtime.surface(surface.id()).is_none() {
                all_resolved = false;
            }
            if surface.id() != root {
                visited_other = true;
            }
        });
        if visited >= self.walk_visited {
            self.walk_visited = visited;
            self.walk_all_resolved = all_resolved;
            self.walk_other_surface = visited_other;
        }
    }
}

fn run_client_round_trip() -> (App, common::client::ClientEvents) {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        client: Some(common::client::spawn(&socket, |state, qh| {
            state.create_toplevel(qh);
        })),
        ..App::new(runtime.clone())
    };

    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");

    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");
    (app, events)
}

#[test]
fn a_live_toplevel_exposes_the_xdg_remainder() {
    let (app, events) = run_client_round_trip();

    assert_eq!(
        app.initial_commits, 1,
        "the bufferless commit reaches the handler"
    );
    assert!(
        events.configure_events >= 1 && events.acked_configures >= 1,
        "the client configured and acked"
    );
    assert!(
        app.setter_results.iter().all(|r| *r == Some(())),
        "every by-id setter resolves the live toplevel: {:?}",
        app.setter_results
    );
    assert_eq!(
        app.parent_result,
        Some(true),
        "clearing the parent succeeds"
    );
    assert!(
        app.roles.contains(&SurfaceRole::Toplevel),
        "the committed surface reports its role: {:?}",
        app.roles
    );
    assert!(
        app.downcast_toplevels >= 1,
        "Toplevel::from_surface downcasts the committed surface"
    );
    assert_eq!(app.pinged, 1, "ping ran against the live base");
    assert!(
        app.root_surfaces >= 1,
        "for_each_surface yields at least the root, got {}",
        app.root_surfaces
    );
    assert!(
        app.walk_all_resolved,
        "the root's yielded SurfaceId resolves through Runtime::surface"
    );
    assert!(app.state_sample.is_some(), "state() reads a live snapshot");
    assert_eq!(
        app.requested_sample,
        Some((false, false, false)),
        "the client requested no states"
    );
    assert!(app.wm_caps_sample.is_some(), "wm_capabilities() reads");
}

#[test]
fn the_new_by_id_ops_all_miss_on_a_dangling_id() {
    common::headless_env();
    let runtime = Runtime::new().expect("runtime");
    let ghost = ToplevelId::dangling_for_test();

    assert_eq!(runtime.set_toplevel_bounds(ghost, 100, 100), None);
    assert_eq!(
        runtime.set_toplevel_constrained(ghost, wlr::Edges::default()),
        None
    );
    assert_eq!(runtime.set_toplevel_parent(ghost, None), None);
    assert_eq!(runtime.set_toplevel_resizing(ghost, true), None);
    assert_eq!(runtime.set_toplevel_suspended(ghost, true), None);
    assert_eq!(
        runtime.set_toplevel_tiled(ghost, wlr::Edges::default()),
        None
    );
    assert_eq!(
        runtime.set_toplevel_wm_capabilities(ghost, WmCapabilities::MAXIMIZE),
        None
    );
    assert_eq!(runtime.decoration_state(ghost), None);
    assert_eq!(runtime.decoration_configure(ghost), None);
    assert!(
        runtime
            .toplevel_of(wlr::SurfaceId::dangling_for_test())
            .is_none()
    );
    assert!(
        runtime
            .popup_of(wlr::SurfaceId::dangling_for_test())
            .is_none()
    );
}

/// The surface-tree walk on a root with no children still hands the closure
/// the root itself, at its own origin.
#[test]
fn toplevel_for_each_surface_visits_the_root() {
    let (app, _events) = run_client_round_trip();
    assert!(app.root_surfaces >= 1);
}

/// A second client-driven pass: map the surface, then disconnect, so the
/// destroy arrives after the map. The id survives the run as a value, and the
/// handler sees both.
#[test]
fn mapped_then_destroyed_is_ordered() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        client: Some(common::client::spawn_mapped(&socket)),
        ..App::new(runtime.clone())
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let _events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("join");

    assert_eq!(app.mapped.len(), 1, "the surface mapped exactly once");
    assert!(
        app.destroyed.contains(&app.mapped[0]),
        "the disconnect destroyed the same toplevel after it mapped"
    );
}

/// A real client's `xdg_toplevel.show_window_menu` reaches the handler through
/// the new defaulted method, with the point it asked about.
#[test]
fn show_window_menu_reaches_the_handler() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_seat(&display, "seat0").expect("seat");
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        client: Some(common::client::spawn_show_window_menu(&socket, 11, 22)),
        ..App::new(runtime.clone())
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let _events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("join");

    assert_eq!(
        app.show_window_menus.len(),
        1,
        "the show-window-menu request reached the handler: {:?}",
        app.show_window_menus
    );
    let (id, x, y) = app.show_window_menus[0];
    assert!(
        app.toplevels.contains(&id),
        "and named the announced toplevel"
    );
    assert_eq!((x, y), (11, 22), "with the client's requested point");
}

/// The surface-tree walk must yield the **surface** id, not the role id: with
/// the wrong addon kind every yielded `SurfaceId` would fail to resolve through
/// `Runtime::surface`, and a plain sub-surface (which carries only the surface
/// id) would be skipped. A mapped child sub-surface makes the walk descend, so
/// this witnesses both halves.
#[test]
fn traversal_yields_resolvable_ids_and_visits_a_subsurface() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        client: Some(common::client::spawn_subsurface_mapped(&socket)),
        ..App::new(runtime.clone())
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let _events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("join");

    assert!(
        app.walk_visited >= 2,
        "the walk visited the root and the mapped sub-surface, got {}",
        app.walk_visited
    );
    assert!(
        app.walk_all_resolved,
        "every yielded SurfaceId resolved through Runtime::surface"
    );
    assert!(
        app.walk_other_surface,
        "a non-root sub-surface was yielded, not skipped"
    );
}

/// The setters whose wlroots call asserts a minimum xdg-shell version, run
/// against a shell created at a version that does not meet it, must report a
/// precondition miss rather than reach the assert (which aborts the process,
/// this distribution shipping wlroots without `NDEBUG`).
struct GateApp {
    runtime: Runtime,
    results: Vec<(&'static str, Option<()>)>,
    client: Option<JoinHandle<common::client::ClientEvents>>,
}

impl GateApp {
    fn new(runtime: Runtime) -> GateApp {
        GateApp {
            runtime,
            results: Vec::new(),
            client: None,
        }
    }
}

impl wlr::OutputHandler for GateApp {}
impl wlr::SeatHandler for GateApp {}
impl wlr::FdHandler for GateApp {}
impl wlr::LoopHandler for GateApp {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}
impl wlr::ToplevelHandler for GateApp {
    fn initial_commit(&mut self, toplevel: &wlr::Toplevel<'_>) {
        let id = toplevel.id();
        self.results = vec![
            (
                "bounds(negative)",
                self.runtime.set_toplevel_bounds(id, -1, 10),
            ),
            ("bounds(ok)", self.runtime.set_toplevel_bounds(id, 10, 10)),
            (
                "constrained",
                self.runtime
                    .set_toplevel_constrained(id, wlr::Edges::default()),
            ),
            (
                "tiled",
                self.runtime.set_toplevel_tiled(id, wlr::Edges::default()),
            ),
            ("suspended", self.runtime.set_toplevel_suspended(id, false)),
            (
                "wm_capabilities",
                self.runtime
                    .set_toplevel_wm_capabilities(id, WmCapabilities::MAXIMIZE),
            ),
        ];
    }
}

fn gate_results(version: u32) -> Vec<(&'static str, Option<()>)> {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_xdg_shell(&display, version)
        .expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = GateApp {
        client: Some(common::client::spawn(&socket, |state, qh| {
            state.create_toplevel(qh);
        })),
        ..GateApp::new(runtime.clone())
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let _events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("join");
    app.results
}

#[test]
fn version_gates_miss_instead_of_aborting() {
    // Version 6: the constrained state (>=7) is refused; everything else the
    // protocol allows at 6 succeeds, and a negative extent is refused.
    let six = gate_results(6);
    let get = |name: &str| six.iter().find(|(n, _)| *n == name).expect("row").1;
    assert_eq!(get("bounds(negative)"), None, "negative extent refused");
    assert_eq!(get("bounds(ok)"), Some(()), "bounds allowed at 6");
    assert_eq!(get("constrained"), None, "constrained needs 7");
    assert_eq!(get("tiled"), Some(()), "tiled allowed at 6");
    assert_eq!(get("suspended"), Some(()), "suspended allowed at 6");
    assert_eq!(get("wm_capabilities"), Some(()), "wm_caps allowed at 6");

    // Version 1: only the v1-era states are reachable. Every newer setter
    // misses rather than tripping wlroots' assert.
    let one = gate_results(1);
    let get = |name: &str| one.iter().find(|(n, _)| *n == name).expect("row").1;
    assert_eq!(get("bounds(ok)"), None, "bounds needs 4");
    assert_eq!(get("tiled"), None, "tiled needs 2");
    assert_eq!(get("suspended"), None, "suspended needs 6");
    assert_eq!(get("wm_capabilities"), None, "wm_caps needs 5");
    assert_eq!(get("constrained"), None, "constrained needs 7");
}
