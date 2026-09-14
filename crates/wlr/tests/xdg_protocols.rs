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

use wlr::{Backend, Display, ForeignExportInfo, Runtime, ToplevelId, Until};

// ---------------------------------------------------------------------------
// Activation: client-free token lifecycle
// ---------------------------------------------------------------------------

/// The token handle owns what it mints: `create`/`add_token` register a
/// redeemable name, `get_name`/`find_token` read it back, and dropping the
/// handle withdraws it.
#[test]
fn activation_tokens_are_owned_and_looked_up_by_name() {
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
/// require a registry, and an export is owned: dropping it withdraws the
/// handle and releases its memory.
#[test]
fn foreign_registry_and_export_are_owned() {
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
        runtime.export_foreign(None).is_none(),
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

    // An export without a toplevel is still a registry entry with a handle.
    let exported = runtime.export_foreign(None).expect("an export");
    let handle = exported.handle().expect("the export has a handle");
    assert!(!handle.is_empty(), "the handle is a non-empty string");
    assert_eq!(exported.toplevel_id(), None);
    assert_eq!(
        runtime.find_foreign_exported(&handle),
        Some(ForeignExportInfo {
            handle: handle.clone(),
            toplevel: None,
        }),
        "the registry finds the live export by handle"
    );

    // A toplevel id that names nothing withdraws the entry rather than
    // creating one that cannot resolve.
    assert!(
        runtime
            .export_foreign(Some(ToplevelId::dangling_for_test()))
            .is_none(),
        "an unknown toplevel id yields no export"
    );

    drop(exported);
    assert!(
        runtime.find_foreign_exported(&handle).is_none(),
        "dropping the export withdraws its handle"
    );

    // Repeated create/drop proves the allocate/init/finish/dealloc cycle is
    // balanced; under ASan a double free or use-after-free is reported.
    for _ in 0..16 {
        let owned = runtime.export_foreign(None).expect("an export");
        assert!(owned.handle().is_some());
    }
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
    /// toplevel is alive, so no pointer in the entry can dangle.
    fn round_trip_foreign(&mut self, id: ToplevelId) {
        let Some(exported) = self.runtime.export_foreign(Some(id)) else {
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
