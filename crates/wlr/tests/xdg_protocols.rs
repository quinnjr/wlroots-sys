//! The M9 protocol-family wrappers — xdg-activation tokens, xdg-dialog,
//! xdg-foreign and xdg-system-bell — against a real (headless) compositor.
//!
//! The activation token has a genuine client-driven round trip: a
//! `wayland-client` connection mints a token, receives its server-generated
//! name, and redeems it, which lands in [`wlr::SeatHandler::request_activate`].
//! The dialog downcast and the foreign export round trip are exercised on a
//! live toplevel the same way. The system bell manager has no client bindings
//! in this wayland-protocols revision, so only its create/double-create
//! contract is covered here; the ring delivery shares the manager-wiring code
//! path the other managers' tests already exercise.

mod common;

use std::thread::JoinHandle;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::dialog::v1::client::{xdg_dialog_v1, xdg_wm_dialog_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols::xdg::system_bell::v1::client::xdg_system_bell_v1;
use wlr::{Backend, Display, Runtime, ToplevelId, Until};

// ---------------------------------------------------------------------------
// Activation: client-free token lifecycle
// ---------------------------------------------------------------------------

/// The token handle owns what it mints: `create`/`add_token` register a
/// redeemable name, `get_name`/`find_token` read it back, and dropping the
/// handle withdraws it.
#[test]
fn activation_tokens_are_owned_and_looked_up_by_name() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    // Before the manager exists there is nothing to mint against, and every
    // accessor reports the missing-global miss rather than a wrong default.
    assert!(
        runtime.create_activation_token().is_none(),
        "no manager, no token"
    );
    assert!(runtime.add_activation_token("token").is_none());
    assert!(runtime.find_activation_token("token").is_none());

    runtime
        .create_xdg_activation_manager(&display)
        .expect("activation manager");
    assert!(
        runtime.create_xdg_activation_manager(&display).is_err(),
        "a second manager is refused"
    );

    // `create` mints a token whose name wlroots generated.
    let created = runtime.create_activation_token().expect("a fresh token");
    let created_name = created.name().expect("the token has a name");
    assert!(
        !created_name.is_empty(),
        "wlroots generated a non-empty name"
    );
    assert_eq!(
        runtime.find_activation_token(&created_name),
        created.snapshot(),
        "a created token is findable under the name it reports"
    );

    // `add_token` adopts a caller-chosen name, and the handle owns membership:
    // dropping it withdraws the name.
    let added = runtime
        .add_activation_token("caller-chosen")
        .expect("an added token");
    assert_eq!(added.name().as_deref(), Some("caller-chosen"));
    assert!(runtime.find_activation_token("caller-chosen").is_some());
    drop(added);
    assert!(
        runtime.find_activation_token("caller-chosen").is_none(),
        "dropping the handle withdraws the adopted name"
    );

    // A token with a NUL is rejected rather than truncated.
    assert!(runtime.add_activation_token("bad\0name").is_none());
    assert!(runtime.find_activation_token("bad\0name").is_none());
}

/// The dialog manager creates once and refuses a second create, and the
/// by-id downcast misses on an unknown toplevel without dereferencing.
#[test]
fn dialog_manager_creates_once_and_downcast_misses_cleanly() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    assert!(
        runtime.dialog(ToplevelId::dangling_for_test()).is_none(),
        "no manager, and a dangling id, both miss"
    );

    runtime
        .create_xdg_dialog_manager(&display, 1)
        .expect("dialog manager");
    assert!(
        runtime.create_xdg_dialog_manager(&display, 1).is_err(),
        "a second dialog manager is refused"
    );
    assert!(
        runtime.dialog(ToplevelId::dangling_for_test()).is_none(),
        "an unknown toplevel id still misses"
    );
}

/// The system bell manager creates once and refuses a second create. There is
/// no `xdg-system-bell-v1` client binding in this wayland-protocols revision,
/// so ring delivery is not driven here.
#[test]
fn system_bell_manager_creates_once() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    runtime
        .create_xdg_system_bell(&display, 1)
        .expect("system bell manager");
    assert!(
        runtime.create_xdg_system_bell(&display, 1).is_err(),
        "a second system bell manager is refused"
    );
}

// ---------------------------------------------------------------------------
// Foreign: registry, managers, export round trip
// ---------------------------------------------------------------------------

/// The registry and both version managers create once each, the v1/v2 managers
/// require a registry, and an export needs a live toplevel (so no targetless
/// entry can ever reach the registry and later be imported).
#[test]
fn foreign_registry_and_managers_are_created_once() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    // The version managers need a registry first.
    assert!(
        runtime.create_xdg_foreign_v1(&display).is_err(),
        "v1 before the registry is refused"
    );
    assert!(
        runtime.create_xdg_foreign_v2(&display).is_err(),
        "v2 before the registry is refused"
    );
    assert!(
        runtime
            .export_foreign(ToplevelId::dangling_for_test())
            .is_none(),
        "no registry, no export"
    );
    assert!(runtime.find_foreign_exported("anything").is_none());

    runtime
        .create_xdg_foreign_registry(&display)
        .expect("registry");
    assert!(
        runtime.create_xdg_foreign_registry(&display).is_err(),
        "a second registry is refused"
    );
    runtime.create_xdg_foreign_v1(&display).expect("foreign v1");
    runtime.create_xdg_foreign_v2(&display).expect("foreign v2");
    assert!(runtime.create_xdg_foreign_v1(&display).is_err());
    assert!(runtime.create_xdg_foreign_v2(&display).is_err());

    // An export requires a live toplevel: an unknown id yields nothing, so no
    // targetless entry can be created that a later import would dereference.
    assert!(
        runtime
            .export_foreign(ToplevelId::dangling_for_test())
            .is_none(),
        "an unknown toplevel id yields no export"
    );
    assert!(runtime.find_foreign_exported("anything").is_none());
    // An interior NUL cannot reach wlroots: the lookup is refused, not
    // truncated, and still misses.
    assert!(
        runtime.find_foreign_exported("a\0b").is_none(),
        "a handle with an interior NUL is refused"
    );
}

// ---------------------------------------------------------------------------
// Client-driven: activation round trip + foreign export of a live toplevel
// ---------------------------------------------------------------------------

/// Records what each handler call observed, and owns the client thread.
struct App {
    runtime: Runtime,
    client: Option<JoinHandle<common::client::ClientEvents>>,
    activations: Vec<Option<ToplevelId>>,
    /// The handle string an export of the live toplevel produced, and whether
    /// the registry resolved it back to the same toplevel while it was live.
    foreign_handle: Option<String>,
    foreign_found: Option<Option<ToplevelId>>,
    foreign_gone_after_drop: Option<bool>,
}

impl App {
    fn new(runtime: Runtime) -> App {
        App {
            runtime,
            client: None,
            activations: Vec::new(),
            foreign_handle: None,
            foreign_found: None,
            foreign_gone_after_drop: None,
        }
    }

    /// Export the live toplevel, read it back through the registry, then drop
    /// the export and confirm the handle stops resolving — all while the
    /// toplevel is alive. The repeated export/drop loop proves the
    /// allocate/init/finish/dealloc cycle is balanced (ASan/LSan is the oracle).
    fn round_trip_foreign(&mut self, id: ToplevelId) {
        for _ in 0..16 {
            let Some(owned) = self.runtime.export_foreign(id) else {
                return;
            };
            assert!(owned.is_alive(), "a live toplevel yields a live export");
            assert!(owned.handle().is_some());
            assert_eq!(owned.toplevel_id(), id);
        }

        let Some(exported) = self.runtime.export_foreign(id) else {
            return;
        };
        let Some(handle) = exported.handle() else {
            return;
        };
        let found = self
            .runtime
            .find_foreign_exported(&handle)
            .map(|info| info.toplevel);
        self.foreign_handle = Some(handle.clone());
        self.foreign_found = found;
        drop(exported);
        self.foreign_gone_after_drop = Some(self.runtime.find_foreign_exported(&handle).is_none());
    }
}

impl wlr::OutputHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

impl wlr::SeatHandler for App {
    fn request_activate(&mut self, target: Option<ToplevelId>, _token: wlr::ActivationToken) {
        self.activations.push(target);
    }
}

impl wlr::ToplevelHandler for App {
    fn initial_commit(&mut self, toplevel: &wlr::Toplevel<'_>) {
        let id = toplevel.id();
        self.round_trip_foreign(id);
    }
}

/// A real client mints an activation token, gets its name back, and redeems it
/// — the server's `request_activate` firing is the round trip's proof.
#[test]
fn activation_token_round_trips_from_a_client() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_xdg_activation_manager(&display)
        .expect("activation manager");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        client: Some(common::client::spawn_activation_round_trip(&socket)),
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

    assert!(
        events.activation_token_received,
        "the client received the token name the server generated"
    );
    assert!(events.activation_sent, "the client redeemed the token");
    assert_eq!(
        app.activations.len(),
        1,
        "the redemption reached request_activate exactly once"
    );
    assert_eq!(
        app.activations[0], None,
        "the activated surface was a plain surface, not a tracked toplevel"
    );
}

/// A live toplevel is exported into the registry, found back by handle, and
/// withdrawn — with every step running while the toplevel is alive.
#[test]
fn foreign_export_round_trips_a_live_toplevel() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_xdg_foreign_registry(&display)
        .expect("registry");
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
    let _events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    let handle = app
        .foreign_handle
        .clone()
        .expect("the toplevel was exported while live");
    assert!(!handle.is_empty(), "the export produced a non-empty handle");
    assert!(
        app.foreign_found
            .expect("the registry was queried")
            .is_some(),
        "the registry resolved the handle to the exported toplevel"
    );
    assert_eq!(
        app.foreign_gone_after_drop,
        Some(true),
        "dropping the export withdrew the handle"
    );
}

/// A compositor export is withdrawn when its toplevel dies first, so neither a
/// later `find_foreign_exported` nor the handle's own `Drop` dereferences the
/// freed toplevel. The export is held across the run deliberately so the death
/// happens while the handle is still alive.
#[test]
fn foreign_export_is_withdrawn_when_its_toplevel_dies() {
    struct DeathApp {
        runtime: Runtime,
        client: Option<JoinHandle<common::client::ClientEvents>>,
        exported: Option<wlr::ForeignExported>,
        handle: Option<String>,
    }
    impl wlr::OutputHandler for DeathApp {}
    impl wlr::FdHandler for DeathApp {}
    impl wlr::SeatHandler for DeathApp {}
    impl wlr::LoopHandler for DeathApp {
        fn should_stop(&mut self) -> bool {
            self.client.as_ref().is_some_and(|h| h.is_finished())
        }
    }
    impl wlr::ToplevelHandler for DeathApp {
        fn initial_commit(&mut self, toplevel: &wlr::Toplevel<'_>) {
            if let Some(exported) = self.runtime.export_foreign(toplevel.id()) {
                self.handle = exported.handle();
                self.exported = Some(exported);
            }
        }
    }

    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_xdg_foreign_registry(&display)
        .expect("registry");
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = DeathApp {
        runtime: runtime.clone(),
        client: Some(common::client::spawn(&socket, |state, qh| {
            state.create_toplevel(qh);
        })),
        exported: None,
        handle: None,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let _events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    let handle = app
        .handle
        .clone()
        .expect("the toplevel was exported while live");
    let exported = app.exported.take().expect("the export was kept");
    assert!(
        !exported.is_alive(),
        "the toplevel's destruction withdrew the export"
    );
    // The lookup must not dereference the freed toplevel: the entry is gone.
    assert!(
        app.runtime.find_foreign_exported(&handle).is_none(),
        "a withdrawn export does not resolve"
    );
    // Dropping must not finish an already-withdrawn entry or touch the freed
    // toplevel. Under ASan a stray dereference or double free would be caught.
    drop(exported);
}

/// The dialog role downcast runs against a real committed toplevel and misses
/// cleanly, since no client bound `xdg_wm_dialog_v1`.
#[test]
fn dialog_downcast_misses_on_a_live_toplevel() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_xdg_dialog_manager(&display, 1)
        .expect("dialog manager");
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    struct DialogApp {
        runtime: Runtime,
        client: Option<JoinHandle<common::client::ClientEvents>>,
        saw_toplevel: bool,
        dialog: Option<bool>,
        dialog_of: Option<bool>,
    }
    impl wlr::OutputHandler for DialogApp {}
    impl wlr::FdHandler for DialogApp {}
    impl wlr::SeatHandler for DialogApp {}
    impl wlr::LoopHandler for DialogApp {
        fn should_stop(&mut self) -> bool {
            self.client.as_ref().is_some_and(|h| h.is_finished())
        }
    }
    impl wlr::ToplevelHandler for DialogApp {
        fn initial_commit(&mut self, toplevel: &wlr::Toplevel<'_>) {
            self.saw_toplevel = true;
            let id = toplevel.id();
            self.dialog = Some(toplevel.dialog().is_none());
            self.dialog_of = Some(self.runtime.dialog(id).is_none());
        }
    }

    let mut app = DialogApp {
        runtime: runtime.clone(),
        client: Some(common::client::spawn(&socket, |state, qh| {
            state.create_toplevel(qh);
        })),
        saw_toplevel: false,
        dialog: None,
        dialog_of: None,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let _events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert!(app.saw_toplevel, "a real toplevel committed");
    assert_eq!(
        app.dialog,
        Some(true),
        "the toplevel carries no dialog role"
    );
    assert_eq!(app.dialog_of, Some(true), "and the by-id path misses too");
}

// ---------------------------------------------------------------------------
// Live legs: a client marks a dialog modal; a client rings the system bell
// ---------------------------------------------------------------------------

/// Test-local client state shared by the dialog and bell legs.
/// `common::client::ClientState` cannot grow these bindings from here (its
/// `Dispatch` impls live in `common/`), so this file owns the small state its
/// own drivers need. wayland-protocols 0.32 *does* ship the staging dialog
/// and system-bell bindings, so the generated types are used here with these
/// local impls — no raw-opcode dispatch was needed.
struct LiveState;

macro_rules! live_empty_dispatch {
    ($($t:ty),+) => {$(
        impl Dispatch<$t, ()> for LiveState {
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

live_empty_dispatch!(
    wl_compositor::WlCompositor,
    wl_surface::WlSurface,
    xdg_toplevel::XdgToplevel,
    xdg_dialog_v1::XdgDialogV1,
    xdg_wm_dialog_v1::XdgWmDialogV1,
    xdg_system_bell_v1::XdgSystemBellV1
);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for LiveState {
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

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for LiveState {
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

impl Dispatch<xdg_surface::XdgSurface, ()> for LiveState {
    fn event(
        _state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
        }
    }
}

/// Spawn the dialog driver: bind `xdg_wm_dialog_v1`, mark the new toplevel
/// modal *before* its first commit (the role exists from `get_xdg_dialog`
/// on), commit, then clear the flag with `unset_modal` and commit again so
/// the server observes both edges.
fn spawn_dialog_client(socket: &str) -> JoinHandle<()> {
    let path = common::isolated_runtime_dir().join(socket);
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
            registry_queue_init::<LiveState>(&conn).expect("registry queue init");
        let qh = queue.handle();
        let mut state = LiveState;

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let dialog_manager: xdg_wm_dialog_v1::XdgWmDialogV1 =
            globals.bind(&qh, 1..=1, ()).expect("bind xdg_wm_dialog_v1");

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        let dialog = dialog_manager.get_xdg_dialog(&toplevel, &qh, ());
        dialog.set_modal();
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the modal dialog");
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the configure is dispatched and acked");

        dialog.unset_modal();
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees unset_modal");
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server drains");

        drop((
            dialog,
            surface,
            xdg_surface,
            toplevel,
            dialog_manager,
            wm_base,
            compositor,
        ));
    })
}

/// Spawn the bell driver: commit a tracked toplevel surface, then ring once
/// naming it and once naming nothing. A hand-rolled `Dispatch` for
/// `xdg_system_bell_v1` (the `LiveState` impl above) is all the binding the
/// generated `XdgSystemBellV1` type needs — no harness change.
fn spawn_bell_client(socket: &str) -> JoinHandle<()> {
    let path = common::isolated_runtime_dir().join(socket);
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
            registry_queue_init::<LiveState>(&conn).expect("registry queue init");
        let qh = queue.handle();
        let mut state = LiveState;

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let bell: xdg_system_bell_v1::XdgSystemBellV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind xdg_system_bell_v1");

        // A tracked surface to name: the bufferless toplevel commit is what
        // the server's surface table records.
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server tracks the surface");

        bell.ring(Some(&surface));
        bell.ring(None);
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees both rings");
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server drains");

        drop((surface, xdg_surface, _toplevel, bell, wm_base, compositor));
    })
}

struct DialogLiveApp {
    runtime: Runtime,
    client: Option<JoinHandle<()>>,
    id: Option<ToplevelId>,
    surface_id: Option<wlr::SurfaceId>,
    /// `toplevel.dialog()` on the live toplevel at `initial_commit`.
    initial_dialog: Option<bool>,
    /// Its `modal()` then: the client marked it before the first commit.
    initial_modal: Option<bool>,
    /// Its `toplevel_id() == id` then.
    initial_toplevel_match: Option<bool>,
    /// Whether any server turn resolved `runtime.dialog(id)`.
    saw_runtime_dialog: bool,
    /// Whether any server turn resolved `runtime.dialog_of(surface)`.
    saw_dialog_of: bool,
    /// Whether `modal()` ever read true on the live dialog.
    saw_modal_true: bool,
    /// The last `modal()` reading: `Some(false)` once `unset_modal` landed.
    last_modal: Option<bool>,
}

impl wlr::OutputHandler for DialogLiveApp {}
impl wlr::FdHandler for DialogLiveApp {}
impl wlr::SeatHandler for DialogLiveApp {}

impl wlr::LoopHandler for DialogLiveApp {
    fn should_stop(&mut self) -> bool {
        // Sample every turn: the modal flag is set before the first commit
        // and cleared before the last, and no handler callback fires for
        // either edge, so polling the live role is the observation path.
        if let Some(id) = self.id {
            if let Some(dialog) = self.runtime.dialog(id) {
                self.saw_runtime_dialog = true;
                if dialog.modal() {
                    self.saw_modal_true = true;
                }
                self.last_modal = Some(dialog.modal());
            }
            if let Some(sid) = self.surface_id
                && self.runtime.dialog_of(sid).is_some()
            {
                self.saw_dialog_of = true;
            }
        }
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

impl wlr::ToplevelHandler for DialogLiveApp {
    fn initial_commit(&mut self, toplevel: &wlr::Toplevel<'_>) {
        let id = toplevel.id();
        self.id = Some(id);
        if let Some(dialog) = toplevel.dialog() {
            self.initial_dialog = Some(true);
            self.initial_modal = Some(dialog.modal());
            self.initial_toplevel_match = Some(dialog.toplevel_id() == id);
        } else {
            self.initial_dialog = Some(false);
        }
    }

    fn surface_committed(&mut self, surface: &wlr::Surface<'_>) {
        if self.surface_id.is_none() && wlr::Toplevel::from_surface(surface).is_some() {
            self.surface_id = Some(surface.id());
        }
    }
}

/// A real client binds `xdg_wm_dialog_v1` and marks its toplevel modal; every
/// by-handle and by-id downcast resolves it, `toplevel_id()` names the
/// toplevel, and `modal()` reads true then false across `unset_modal`.
#[test]
fn dialog_round_trips_on_a_live_dialog() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_xdg_dialog_manager(&display, 1)
        .expect("dialog manager");
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = DialogLiveApp {
        runtime: runtime.clone(),
        client: Some(spawn_dialog_client(&socket)),
        id: None,
        surface_id: None,
        initial_dialog: None,
        initial_modal: None,
        initial_toplevel_match: None,
        saw_runtime_dialog: false,
        saw_dialog_of: false,
        saw_modal_true: false,
        last_modal: None,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert!(app.id.is_some(), "a real toplevel committed");
    assert_eq!(
        app.initial_dialog,
        Some(true),
        "toplevel.dialog() resolves the live dialog"
    );
    assert_eq!(
        app.initial_modal,
        Some(true),
        "the dialog is modal at the first commit"
    );
    assert_eq!(
        app.initial_toplevel_match,
        Some(true),
        "dialog.toplevel_id() names the live toplevel"
    );
    assert!(
        app.saw_runtime_dialog,
        "runtime.dialog(id) resolves the live dialog"
    );
    assert!(
        app.saw_dialog_of,
        "runtime.dialog_of(surface) resolves the live dialog"
    );
    assert!(
        app.saw_modal_true,
        "modal() read true while the client had it set"
    );
    assert_eq!(
        app.last_modal,
        Some(false),
        "modal() reads false after unset_modal"
    );
}

struct BellApp {
    runtime: Runtime,
    client: Option<JoinHandle<()>>,
    rings: Vec<Option<wlr::SurfaceId>>,
    /// Whether the surface the first ring named resolved through
    /// `Runtime::surface` while live (sampled in the handler: the surface is
    /// gone by the time the run returns).
    first_ring_resolved: Option<bool>,
}

impl wlr::OutputHandler for BellApp {}
impl wlr::FdHandler for BellApp {}
impl wlr::SeatHandler for BellApp {}

impl wlr::LoopHandler for BellApp {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

impl wlr::ToplevelHandler for BellApp {
    fn system_bell_ring(&mut self, surface: Option<wlr::SurfaceId>) {
        if self.rings.is_empty() {
            self.first_ring_resolved =
                Some(surface.is_some_and(|id| self.runtime.surface(id).is_some()));
        }
        self.rings.push(surface);
    }
}

/// A real client rings the system bell naming its surface and then naming
/// nothing; both deliveries reach `system_bell_ring`, and the named surface
/// resolves while live.
#[test]
fn a_client_ring_reaches_the_handler() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_xdg_system_bell(&display, 1)
        .expect("system bell manager");
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = BellApp {
        runtime: runtime.clone(),
        client: Some(spawn_bell_client(&socket)),
        rings: Vec::new(),
        first_ring_resolved: None,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(app.rings.len(), 2, "both rings reached the handler");
    assert!(
        app.rings[0].is_some(),
        "the first ring named the client's surface"
    );
    assert_eq!(app.rings[1], None, "the second ring named no surface");
    assert_eq!(
        app.first_ring_resolved,
        Some(true),
        "the named surface resolved through Runtime::surface while live"
    );
}
