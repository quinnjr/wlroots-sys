//! `foreign-toplevel-management`: the manager global, the owned
//! `ForeignToplevelHandle`, and the client requests it forwards.
//!
//! Wlroots keeps this protocol in the `wlr-protocols` set rather than
//! `wayland-protocols`, so the client-driven leg binds
//! `zwlr_foreign_toplevel_manager_v1` through the `wayland-protocols-wlr`
//! companion crate.

mod common;

use std::thread::JoinHandle;

use wlr::{
    Backend, Display, ForeignToplevelHandle, ForeignToplevelId, ForeignToplevelState, SurfaceId,
    Until,
};

#[derive(Debug, Default, PartialEq, Eq)]
struct Requests {
    activate: u32,
    close: u32,
    close_id: Option<ForeignToplevelId>,
    maximize: Vec<bool>,
    minimize: Vec<bool>,
    fullscreen: Vec<bool>,
    rectangles: Vec<(Option<SurfaceId>, i32, i32, i32, i32)>,
}

struct App {
    client: Option<JoinHandle<common::client::ForeignToplevelEvents>>,
    handle: Option<ForeignToplevelHandle>,
    requests: Requests,
}

impl App {
    fn new(handle: Option<ForeignToplevelHandle>) -> Self {
        Self {
            client: None,
            handle,
            requests: Requests::default(),
        }
    }
}

impl wlr::OutputHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::ToplevelHandler for App {
    fn foreign_toplevel_activate(&mut self, _id: ForeignToplevelId) {
        self.requests.activate += 1;
    }

    fn foreign_toplevel_close(&mut self, id: ForeignToplevelId) {
        self.requests.close += 1;
        self.requests.close_id = Some(id);
        // Drop the owned handle from inside the request handler. `Drop` must
        // defer the wlroots destroy until after wlroots finishes emitting this
        // very signal, or it frees the signal mid-emission.
        self.handle.take();
    }

    fn foreign_toplevel_maximize(&mut self, _id: ForeignToplevelId, maximized: bool) {
        self.requests.maximize.push(maximized);
    }

    fn foreign_toplevel_minimize(&mut self, _id: ForeignToplevelId, minimized: bool) {
        self.requests.minimize.push(minimized);
    }

    fn foreign_toplevel_fullscreen(&mut self, _id: ForeignToplevelId, fullscreen: bool) {
        self.requests.fullscreen.push(fullscreen);
    }

    fn foreign_toplevel_set_rectangle(
        &mut self,
        _id: ForeignToplevelId,
        surface: Option<SurfaceId>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        self.requests
            .rectangles
            .push((surface, x, y, width, height));
    }
}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

/// The manager global creates once and refuses a second create, and the handle
/// factories report the missing-manager miss rather than a wrong default.
#[test]
fn manager_creates_once_and_handles_need_it() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");

    assert!(
        runtime.create_foreign_toplevel().is_none(),
        "no manager, no handle"
    );

    runtime
        .create_foreign_toplevel_manager(&display)
        .expect("manager");
    assert!(
        runtime.create_foreign_toplevel_manager(&display).is_err(),
        "a second manager is refused"
    );
}

/// A handle tracks the state the compositor reports, and every mutator is
/// reflected by the snapshot.
#[test]
fn handle_state_round_trips_through_the_mutators() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_foreign_toplevel_manager(&display)
        .expect("manager");

    let handle = runtime.create_foreign_toplevel().expect("a fresh handle");
    assert!(handle.is_alive(), "a fresh handle is live");

    let fresh = handle.state();
    assert_eq!(fresh.title, None);
    assert_eq!(fresh.app_id, None);
    assert!(!fresh.maximized && !fresh.minimized && !fresh.activated && !fresh.fullscreen);
    assert_eq!(fresh.parent, None);

    handle.set_title("wlr test window").expect("title");
    handle.set_app_id("org.wlr.test").expect("app id");
    handle.set_maximized(true).expect("maximize");
    handle.set_minimized(true).expect("minimize");
    handle.set_activated(true).expect("activate");
    handle.set_fullscreen(true).expect("fullscreen");

    let state = handle.state();
    assert_eq!(state.title.as_deref(), Some("wlr test window"));
    assert_eq!(state.app_id.as_deref(), Some("org.wlr.test"));
    assert!(state.maximized && state.minimized && state.activated && state.fullscreen);

    handle.set_maximized(false).expect("unmaximize");
    handle.set_minimized(false).expect("unminimize");
    handle.set_activated(false).expect("deactivate");
    handle.set_fullscreen(false).expect("unfullscreen");
    let cleared = handle.state();
    assert!(!cleared.maximized && !cleared.minimized && !cleared.activated && !cleared.fullscreen);

    // An interior NUL is refused rather than truncated.
    assert_eq!(handle.set_title("bad\0title"), None);
    assert_eq!(handle.state().title.as_deref(), Some("wlr test window"));
}

/// Parent/child destroy order is safe in both directions: wlroots reparents a
/// child to NULL when its parent dies, so neither drop may double-free and
/// neither handle may name freed memory afterwards.
#[test]
fn parent_child_destroy_order_is_safe() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_foreign_toplevel_manager(&display)
        .expect("manager");

    for _ in 0..8 {
        let parent = runtime.create_foreign_toplevel().expect("parent");
        let child = runtime.create_foreign_toplevel().expect("child");
        child.set_parent(Some(&parent)).expect("set parent");
        assert_eq!(child.state().parent, Some(parent.id()));

        // Drop the parent first: wlroots rewrites the child's parent to NULL
        // from inside the parent's own destroy.
        drop(parent);
        assert!(child.is_alive(), "the child survives its parent");
        assert_eq!(
            child.state().parent,
            None,
            "the child was reparented to NULL"
        );
        drop(child);

        // The other order: dropping the child leaves the parent valid.
        let parent = runtime.create_foreign_toplevel().expect("parent");
        let child = runtime.create_foreign_toplevel().expect("child");
        child.set_parent(Some(&parent)).expect("set parent");
        drop(child);
        assert!(parent.is_alive());
        drop(parent);
    }
}

/// `output_enter`/`output_leave` are safe against a live output, and the
/// round trip leaves the handle usable.
#[test]
fn output_enter_leave_are_safe_on_a_live_handle() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_foreign_toplevel_manager(&display)
        .expect("manager");
    let handle = runtime.create_foreign_toplevel().expect("handle");

    struct Probe {
        handle: Option<ForeignToplevelHandle>,
        seen: bool,
    }
    impl wlr::OutputHandler for Probe {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            if let Some(handle) = &self.handle {
                handle.output_enter(output);
                handle.output_leave(output);
                self.seen = true;
            }
        }
    }
    impl wlr::ToplevelHandler for Probe {}
    impl wlr::SeatHandler for Probe {}
    impl wlr::FdHandler for Probe {}
    impl wlr::LoopHandler for Probe {}

    let mut probe = Probe {
        handle: Some(handle),
        seen: false,
    };
    backend
        .run_all(&display, &mut probe, &runtime, Until::Turns(4))
        .expect("run_all");
    assert!(probe.seen, "the live output reached output_enter/leave");
    assert!(probe.handle.as_ref().expect("handle").is_alive());
}

/// A real client binds `zwlr_foreign_toplevel_manager_v1`, observes the handle
/// the compositor exported (title and app id included), and drives every
/// request the handle carries — each one landing in `ToplevelHandler`.
#[test]
fn a_client_observes_and_drives_an_exported_handle() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_seat(&display, "seat0").expect("seat");
    runtime
        .create_foreign_toplevel_manager(&display)
        .expect("manager");

    let handle = runtime.create_foreign_toplevel().expect("handle");
    handle.set_title("Wlr Test Window").expect("title");
    handle.set_app_id("org.wlr.test").expect("app id");
    let handle_id = handle.id();

    let socket = display.add_socket_auto().expect("socket");
    let mut app = App::new(Some(handle));
    app.client = Some(common::client::spawn_foreign_toplevel(&socket));

    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(
        events.toplevels_seen, 1,
        "the client saw one exported handle"
    );
    assert_eq!(events.title.as_deref(), Some("Wlr Test Window"));
    assert_eq!(events.app_id.as_deref(), Some("org.wlr.test"));
    assert!(events.saw_done, "the initial done event arrived");
    assert!(events.state_events >= 1, "the initial state array arrived");

    assert_eq!(app.requests.maximize, vec![true]);
    assert_eq!(app.requests.minimize, vec![true, false]);
    assert_eq!(app.requests.fullscreen, vec![true]);
    assert_eq!(app.requests.activate, 1);
    assert_eq!(app.requests.close, 1);
    assert_eq!(
        app.requests.close_id,
        Some(handle_id),
        "the close request named the handle the compositor exported"
    );
    assert_eq!(
        app.requests.rectangles,
        vec![(None, 5, 6, 7, 8)],
        "set_rectangle arrived with its coordinates; the client's surface is \
         not one this crate tracks, so its id is None"
    );

    // The close handler dropped the owned handle from inside the delivery; it
    // must be gone afterwards and the drop must not have freed the request
    // signal wlroots was still emitting.
    assert!(
        app.handle.is_none(),
        "the close handler dropped the exported handle"
    );
}

/// The manager dies with its display before the handle does: the handle's watch
/// marks it inert, so every accessor is a miss and `Drop` does not touch the
/// freed manager.
#[test]
fn dropping_the_display_makes_a_handle_inert() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_foreign_toplevel_manager(&display)
        .expect("manager");
    let handle = runtime.create_foreign_toplevel().expect("handle");
    handle.set_title("live").expect("title");
    assert!(handle.is_alive());
    assert_eq!(handle.title().as_deref(), Some("live"));

    // Destroy the display out from under the handle. wlroots emits the
    // manager's `destroy` while its memory is still valid, and the handle's
    // watch runs there.
    drop(display);

    assert!(!handle.is_alive(), "the manager's death was observed");
    assert!(
        handle.title().is_none(),
        "no accessor dereferences freed memory"
    );
    assert_eq!(handle.state(), ForeignToplevelState::default());
    assert_eq!(handle.set_title("late"), None, "no mutator writes either");
    // Drop is a no-op now instead of a double free against the freed manager.
    drop(handle);
}
