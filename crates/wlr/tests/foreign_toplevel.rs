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

    // The direct accessors agree with the snapshot.
    assert_eq!(handle.title().as_deref(), Some("wlr test window"));
    assert_eq!(handle.app_id().as_deref(), Some("org.wlr.test"));

    handle.set_maximized(false).expect("unmaximize");
    handle.set_minimized(false).expect("unminimize");
    handle.set_activated(false).expect("deactivate");
    handle.set_fullscreen(false).expect("unfullscreen");
    let cleared = handle.state();
    assert!(!cleared.maximized && !cleared.minimized && !cleared.activated && !cleared.fullscreen);

    // An interior NUL is refused rather than truncated.
    assert_eq!(handle.set_title("bad\0title"), None);
    assert_eq!(handle.state().title.as_deref(), Some("wlr test window"));
    assert_eq!(handle.set_app_id("bad\0id"), None);
    assert_eq!(handle.app_id().as_deref(), Some("org.wlr.test"));
    assert_eq!(
        handle.state().app_id.as_deref(),
        Some("org.wlr.test"),
        "a refused app id leaves the previous one in place"
    );
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
                assert!(
                    handle.output_enter(output).is_some(),
                    "output_enter reports the live handle"
                );
                assert!(
                    handle.output_leave(output).is_some(),
                    "output_leave reports the live handle"
                );
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
    // The driven flow ends with the client's own `close()` request, which
    // drops the server-side handle; wlroots answers with `closed`, dispatched
    // by the final round-trip. Asserting it proves the recording arm fires —
    // no request is driven after it, since `close()` was the last one sent.
    assert!(
        events.saw_closed,
        "the server's `closed` answer to the client's own `close()` request must arrive"
    );
    // The bind-time replay sends one `parent` event (naming no parent — the
    // handle is never parented); nothing re-parents it afterwards, so exactly
    // one arrives. Asserting the count proves the recording arm fires instead
    // of swallowing the event.
    assert_eq!(
        events.parent_events, 1,
        "the bind-time replay sends exactly one `parent` event"
    );

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
    // The turn drained without fault — the run above returned `Ok` and the
    // client's `closed` arrived — and the manager still mints handles after
    // an in-handler drop.
    assert!(
        runtime.create_foreign_toplevel().is_some(),
        "a later create works after an in-handler drop drained"
    );
}

/// `set_rectangle` naming a surface the compositor tracks resolves to `Some`:
/// the client builds an xdg toplevel plus a bufferless child subsurface — both
/// announced, so both tracked — and rectangles the child surface.
#[test]
fn set_rectangle_with_a_tracked_surface_resolves_to_some() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    runtime
        .create_foreign_toplevel_manager(&display)
        .expect("manager");

    let handle = runtime.create_foreign_toplevel().expect("handle");
    handle.set_title("Wlr Test Window").expect("title");

    struct App {
        client: Option<JoinHandle<common::client::ForeignToplevelEvents>>,
        handle: Option<ForeignToplevelHandle>,
        rectangles: Vec<(Option<SurfaceId>, i32, i32, i32, i32)>,
        children: Vec<SurfaceId>,
    }
    impl wlr::OutputHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::FdHandler for App {}
    impl wlr::ToplevelHandler for App {
        fn new_subsurface(&mut self, _parent: SurfaceId, child: SurfaceId) {
            self.children.push(child);
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
            self.rectangles.push((surface, x, y, width, height));
        }
        fn foreign_toplevel_close(&mut self, _id: ForeignToplevelId) {
            self.handle.take();
        }
    }
    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            self.client.as_ref().is_some_and(|h| h.is_finished())
        }
    }

    let socket = display.add_socket_auto().expect("socket");
    let mut app = App {
        client: Some(common::client::spawn_foreign_toplevel_tracked_rectangle(
            &socket,
        )),
        handle: Some(handle),
        rectangles: Vec::new(),
        children: Vec::new(),
    };

    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(
        app.children.len(),
        1,
        "the server announced the child subsurface"
    );
    assert_eq!(
        app.rectangles,
        vec![(Some(app.children[0]), 11, 22, 33, 44)],
        "the tracked child surface resolved to its id, unlike the untracked \
         surface of the untracked-rectangle leg"
    );
    assert!(
        app.handle.is_none(),
        "the close handler dropped the exported handle"
    );
}

/// `set_parent` is manager-scoped: a parent from another runtime is refused,
/// while a same-runtime parent still links.
#[test]
fn set_parent_across_runtimes_is_refused() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let first = wlr::Runtime::new().expect("runtime");
    let second = wlr::Runtime::new().expect("runtime");
    first
        .create_foreign_toplevel_manager(&display)
        .expect("manager");
    second
        .create_foreign_toplevel_manager(&display)
        .expect("manager");

    let handle = first.create_foreign_toplevel().expect("handle");
    let other = second.create_foreign_toplevel().expect("other");
    assert_eq!(
        handle.set_parent(Some(&other)),
        None,
        "a parent from another runtime names another manager and is refused"
    );
    assert_eq!(
        handle.state().parent,
        None,
        "the refused parent left no link behind"
    );

    let sibling = first.create_foreign_toplevel().expect("sibling");
    handle
        .set_parent(Some(&sibling))
        .expect("same-runtime parent");
    assert_eq!(handle.state().parent, Some(sibling.id()));
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
    handle.set_app_id("org.wlr.live").expect("app id");
    assert!(handle.is_alive());
    assert_eq!(handle.title().as_deref(), Some("live"));
    assert_eq!(handle.app_id().as_deref(), Some("org.wlr.live"));

    // A second handle parented to the first, so the inert-parent refusal has
    // something to refuse with after the display dies.
    let child = runtime.create_foreign_toplevel().expect("child");
    child.set_parent(Some(&handle)).expect("set parent");
    assert_eq!(child.state().parent, Some(handle.id()));

    // Destroy the display out from under the handle. wlroots emits the
    // manager's `destroy` while its memory is still valid, and the handle's
    // watch runs there.
    drop(display);

    assert!(!handle.is_alive(), "the manager's death was observed");
    assert!(
        handle.title().is_none(),
        "no accessor dereferences freed memory"
    );
    assert!(handle.app_id().is_none(), "the app-id accessor misses too");
    assert_eq!(handle.state(), ForeignToplevelState::default());
    assert_eq!(handle.set_title("late"), None, "no mutator writes either");
    assert_eq!(handle.set_app_id("org.wlr.late"), None);
    assert_eq!(handle.set_maximized(true), None);
    assert_eq!(handle.set_minimized(true), None);
    assert_eq!(handle.set_activated(true), None);
    assert_eq!(handle.set_fullscreen(true), None);
    assert!(
        runtime.create_foreign_toplevel().is_none(),
        "the manager's death cleared the stored pointer, so a post-teardown \
         create misses instead of dereferencing freed memory"
    );
    assert!(
        !child.is_alive(),
        "the child observed the manager's death too"
    );
    assert_eq!(
        child.set_parent(Some(&handle)),
        None,
        "set_parent against an inert parent must refuse rather than touch the freed manager"
    );
    assert_eq!(
        child.set_parent(None),
        None,
        "even clearing the parent is refused once the handle is inert"
    );
    // Drop is a no-op now instead of a double free against the freed manager.
    drop(child);
    drop(handle);
}
