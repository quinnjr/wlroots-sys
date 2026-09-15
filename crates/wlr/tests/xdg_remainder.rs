//! The M9 xdg-shell remainder, against a real headless compositor.
//!
//! A real Wayland client drives a toplevel through bufferless commit so the
//! server's `initial_commit` fires; the handler then exercises every new
//! accessor and by-id setter on the live object, and a second run proves the
//! destroy path delivers after the map. The by-id misses, the role downcast
//! and the surface-tree walk are asserted alongside.

mod common;

use std::thread::JoinHandle;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wlr::{
    Backend, Display, PopupParent, Runtime, SurfaceId, SurfaceRole, ToplevelId, ToplevelState,
    Until, WmCapabilities,
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
    /// The root surface of the live toplevel, captured from the first surface
    /// commit that downcasts to a toplevel — the key the popup-tree probes
    /// below resolve through.
    root_surface: Option<SurfaceId>,
    /// The parent the announced popup named.
    popup_parent: Option<PopupParent>,
    /// How many surfaces `for_each_popup_surface` yielded on the live
    /// toplevel once its popup mapped, and whether every one resolved
    /// through `Runtime::surface`.
    popup_walk: usize,
    popup_walk_resolved: bool,
    /// Walk-yielded non-root surfaces whose `popup_of` named the live
    /// toplevel as parent.
    popup_nonroot_with_parent: usize,
    /// Whether `popup_surface_at` hit inside the mapped popup, and whether
    /// the struck surface's `popup_of` named the live toplevel.
    popup_hit: Option<bool>,
    popup_hit_parent_ok: Option<bool>,
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
            root_surface: None,
            popup_parent: None,
            popup_walk: 0,
            popup_walk_resolved: false,
            popup_nonroot_with_parent: 0,
            popup_hit: None,
            popup_hit_parent_ok: None,
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
        // First surface that is a toplevel root is the popup-tree probe key.
        if self.root_surface.is_none() && wlr::Toplevel::from_surface(surface).is_some() {
            self.root_surface = Some(surface.id());
        }
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

    fn new_popup(&mut self, popup: &wlr::Popup<'_>) {
        self.popup_parent = Some(popup.parent());
    }

    fn popup_mapped(&mut self, id: wlr::PopupId) {
        let Some(root) = self.root_surface else {
            return;
        };
        let Some(top) = self.runtime.toplevel_of(root) else {
            return;
        };
        let live = top.id();
        let mut count = 0usize;
        let mut all_resolved = true;
        let mut nonroot_ok = 0usize;
        top.for_each_popup_surface(|surface, _x, _y| {
            count += 1;
            if self.runtime.surface(surface.id()).is_none() {
                all_resolved = false;
            } else if surface.id() != root
                && self
                    .runtime
                    .popup_of(surface.id())
                    .is_some_and(|popup| popup.parent() == PopupParent::Toplevel(live))
            {
                nonroot_ok += 1;
            }
        });
        self.popup_walk = count;
        self.popup_walk_resolved = all_resolved;
        self.popup_nonroot_with_parent = nonroot_ok;

        // Probe inside the mapped popup: its own (4, 4), expressed in
        // toplevel coordinates, must hit a resolvable surface whose
        // `popup_of` names the live toplevel.
        if let Some(popup) = self.runtime.popup(id) {
            let (tx, ty) = popup.toplevel_coords(4, 4);
            match top.popup_surface_at(tx as f64, ty as f64) {
                Some((hit, _, _)) => {
                    let hid = hit.id();
                    self.popup_hit = Some(self.runtime.surface(hid).is_some());
                    self.popup_hit_parent_ok = Some(
                        self.runtime
                            .popup_of(hid)
                            .is_some_and(|hit| hit.parent() == PopupParent::Toplevel(live)),
                    );
                }
                None => {
                    self.popup_hit = Some(false);
                }
            }
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
    let _serial = common::headless_guard();
    common::headless_env();
    // The shell is created at v7 — the newest version any gated setter needs
    // — so every version guard below passes and a `None` proves the by-id
    // miss rather than an old-shell refusal masking it.
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
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
    /// `set_toplevel_parent(id, Some(id))` on the live toplevel: `Some(false)`
    /// when wlroots refuses the self-parent loop. Kept beside `results`
    /// because the outcome is `Option<bool>`, not `Option<()>`.
    parent_self: Option<Option<bool>>,
    /// `set_toplevel_parent(live_id, Some(dangling))`: `None` for the unknown
    /// parent miss.
    parent_dangling: Option<Option<bool>>,
    client: Option<JoinHandle<common::client::ClientEvents>>,
}

impl GateApp {
    fn new(runtime: Runtime) -> GateApp {
        GateApp {
            runtime,
            results: Vec::new(),
            parent_self: None,
            parent_dangling: None,
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
            (
                "size(negative-w)",
                self.runtime.set_toplevel_size(id, -1, 10),
            ),
            (
                "size(negative-h)",
                self.runtime.set_toplevel_size(id, 10, -1),
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
        // A toplevel cannot parent itself: wlroots refuses the loop with
        // `false` rather than mis-resolving, and an unknown parent is a
        // by-id miss (`None`). Neither disturbs the live toplevel.
        self.parent_self = Some(self.runtime.set_toplevel_parent(id, Some(id)));
        self.parent_dangling = Some(
            self.runtime
                .set_toplevel_parent(id, Some(ToplevelId::dangling_for_test())),
        );
    }
}

fn gate_results(version: u32) -> GateApp {
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
    app
}

#[test]
fn version_gates_miss_instead_of_aborting() {
    // Version 6: the constrained state (>=7) is refused; everything else the
    // protocol allows at 6 succeeds, and a negative extent is refused.
    let six = gate_results(6);
    let get = |name: &str| six.results.iter().find(|(n, _)| *n == name).expect("row").1;
    assert_eq!(get("bounds(negative)"), None, "negative extent refused");
    assert_eq!(get("size(negative-w)"), None, "negative width refused");
    assert_eq!(get("size(negative-h)"), None, "negative height refused");
    assert_eq!(get("bounds(ok)"), Some(()), "bounds allowed at 6");
    assert_eq!(
        six.parent_self,
        Some(Some(false)),
        "a toplevel cannot parent itself: the loop is refused, not missed"
    );
    assert_eq!(
        six.parent_dangling,
        Some(None),
        "an unknown parent is a by-id miss"
    );
    assert_eq!(get("constrained"), None, "constrained needs 7");
    assert_eq!(get("tiled"), Some(()), "tiled allowed at 6");
    assert_eq!(get("suspended"), Some(()), "suspended allowed at 6");
    assert_eq!(get("wm_capabilities"), Some(()), "wm_caps allowed at 6");

    // Version 1: only the v1-era states are reachable. Every newer setter
    // misses rather than tripping wlroots' assert.
    let one = gate_results(1);
    let get = |name: &str| one.results.iter().find(|(n, _)| *n == name).expect("row").1;
    assert_eq!(get("bounds(ok)"), None, "bounds needs 4");
    assert_eq!(get("tiled"), None, "tiled needs 2");
    assert_eq!(get("suspended"), None, "suspended needs 6");
    assert_eq!(get("wm_capabilities"), None, "wm_caps needs 5");
    assert_eq!(get("constrained"), None, "constrained needs 7");
}

// ---------------------------------------------------------------------------
// Popup tree: a live xdg_popup on the toplevel surface
// ---------------------------------------------------------------------------

/// Test-local client state for the popup leg. `common::client::ClientState`
/// cannot grow an `xdg_popup` binding from here (its `Dispatch` impls live in
/// `common/`), so this file owns the small state its own `spawn_popup_client`
/// needs: ack configures and pong the base, like the shared harness.
struct PopupState {
    events: common::client::ClientEvents,
}

macro_rules! popup_empty_dispatch {
    ($($t:ty),+) => {$(
        impl Dispatch<$t, ()> for PopupState {
            fn event(
                _state: &mut Self,
                _proxy: &$t,
                _event: <$t as wayland_client::Proxy>::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    )+};
}

popup_empty_dispatch!(
    wl_compositor::WlCompositor,
    wl_surface::WlSurface,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    xdg_toplevel::XdgToplevel,
    xdg_positioner::XdgPositioner,
    xdg_popup::XdgPopup
);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for PopupState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for PopupState {
    fn event(
        _state: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for PopupState {
    fn event(
        state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.events.configure_events += 1;
            proxy.ack_configure(serial);
            state.events.acked_configures += 1;
        }
    }
}

/// Drive a client that parents a mapped `xdg_popup` on a mapped toplevel:
/// bufferless toplevel commit, map the parent with a 64x64 shm buffer, then
/// create the popup off the parent's `xdg_surface` with a 32x32 buffer and
/// commit it. Each phase is round-tripped so the server has observed it.
fn spawn_popup_client(socket: &str) -> JoinHandle<common::client::ClientEvents> {
    use std::os::fd::AsFd as _;
    let path = common::isolated_runtime_dir().join(socket);
    let shm_path = common::isolated_runtime_dir().join(format!(
        "wlr-rs-shm-{}-{}-popup",
        std::process::id(),
        socket
    ));
    let stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to wayland socket");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("set read timeout on wayland socket");
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("set write timeout on wayland socket");
    std::thread::spawn(move || {
        let conn = Connection::from_socket(stream).expect("wrap wayland socket");
        let (globals, mut queue) =
            registry_queue_init::<PopupState>(&conn).expect("registry queue init");
        let qh = queue.handle();
        let mut state = PopupState {
            events: common::client::ClientEvents::default(),
        };

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");

        // Phase 1: the parent toplevel, committed bufferless so the server
        // announces and tracks it.
        let parent = compositor.create_surface(&qh, ());
        let parent_xdg = wm_base.get_xdg_surface(&parent, &qh, ());
        let _parent_toplevel = parent_xdg.get_toplevel(&qh, ());
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server announces the parent");

        // Phase 2: map the parent with a 64x64 shm buffer.
        const PW: i32 = 64;
        const PH: i32 = 64;
        const CW: i32 = 32;
        const CH: i32 = 32;
        let parent_size = PW * PH * 4;
        let child_size = CW * CH * 4;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&shm_path)
            .expect("create shm backing file");
        file.set_len((parent_size + child_size) as u64)
            .expect("size shm backing file");
        let pool = shm.create_pool(file.as_fd(), parent_size + child_size, &qh, ());
        let parent_buffer =
            pool.create_buffer(0, PW, PH, PW * 4, wl_shm::Format::Argb8888, &qh, ());
        let popup_buffer = pool.create_buffer(
            parent_size,
            CW,
            CH,
            CW * 4,
            wl_shm::Format::Argb8888,
            &qh,
            (),
        );
        parent.attach(Some(&parent_buffer), 0, 0);
        parent.damage(0, 0, PW, PH);
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the parent maps");

        // Phase 3: the popup off the parent's xdg_surface. The first commit
        // is bufferless, like every xdg role's: committing a buffer before
        // the server has configured the popup is a protocol error
        // ("xdg_surface has never been configured") that kills the client.
        // Round-trip until the popup's configure is dispatched and acked
        // (the parent's took the first ack), then the buffered commit maps.
        let popup_surface = compositor.create_surface(&qh, ());
        let popup_xdg = wm_base.get_xdg_surface(&popup_surface, &qh, ());
        let positioner = wm_base.create_positioner(&qh, ());
        positioner.set_size(CW, CH);
        positioner.set_anchor_rect(0, 0, 10, 10);
        let popup = popup_xdg.get_popup(Some(&parent_xdg), &positioner, &qh, ());
        popup_surface.commit();
        for _ in 0..10 {
            if state.events.acked_configures >= 2 {
                break;
            }
            queue
                .roundtrip(&mut state)
                .expect("roundtrip so the popup configure arrives");
        }
        assert!(
            state.events.acked_configures >= 2,
            "the popup was configured and acked before its buffered commit"
        );
        popup_surface.attach(Some(&popup_buffer), 0, 0);
        popup_surface.damage(0, 0, CW, CH);
        popup_surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the popup commit");
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server drains");

        drop((
            popup,
            positioner,
            popup_surface,
            popup_xdg,
            parent,
            parent_xdg,
            _parent_toplevel,
            parent_buffer,
            popup_buffer,
            pool,
            shm,
            file,
            wm_base,
            compositor,
        ));
        state.events
    })
}

/// A real client parents an `xdg_popup` on the live toplevel and maps it; the
/// popup-tree walk, the popup-only hit test and the by-surface downcast all
/// resolve against that toplevel.
#[test]
fn popup_tree_is_walkable_and_hittable_on_a_live_toplevel() {
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
        client: Some(spawn_popup_client(&socket)),
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

    let live = app
        .toplevels
        .first()
        .copied()
        .expect("a toplevel was announced");
    assert_eq!(
        app.popup_parent,
        Some(PopupParent::Toplevel(live)),
        "the popup hangs off the live toplevel"
    );
    assert!(
        app.popup_walk >= 1 && app.popup_walk_resolved,
        "for_each_popup_surface yielded {} surfaces, all resolvable",
        app.popup_walk
    );
    assert!(
        app.popup_nonroot_with_parent >= 1,
        "a non-root walk surface downcasts via popup_of to the live toplevel"
    );
    assert_eq!(
        app.popup_hit,
        Some(true),
        "popup_surface_at hits inside the mapped popup"
    );
    assert_eq!(
        app.popup_hit_parent_ok,
        Some(true),
        "the struck surface's popup_of names the live toplevel"
    );
}
