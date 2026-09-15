//! `wp_security_context_v1` and the session-lock surface completion.
//!
//! The manager create/lookup contract is exercised without a client. The commit
//! path is client-driven: a real `wayland-client` connection binds
//! `wp_security_context_manager_v1`, creates a listener, attaches the metadata
//! and commits — and the server-side handler records the [`SecurityContext`] it
//! was handed. That value owns its strings, so it is read back after the client
//! thread and the display are gone.

mod common;

use std::thread::JoinHandle;

use wlr::{Backend, Display, Runtime, SecurityContext, SurfaceId, ToplevelHandler, Until};

/// Records every committed security context the server-side handler observes,
/// and stops the run once the driving client thread ends.
#[derive(Default)]
struct App {
    client: Option<JoinHandle<()>>,
    contexts: Vec<SecurityContext>,
}

impl wlr::OutputHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}

impl ToplevelHandler for App {
    fn security_context_committed(&mut self, context: &SecurityContext) {
        self.contexts.push(context.clone());
    }
}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

/// The manager global creates once and refuses a second create; a lookup with
/// no client misses before and after it exists, and never touches a null
/// pointer.
#[test]
fn security_context_manager_creates_once_and_lookup_misses() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    assert!(
        // SAFETY: a null client is the explicit "no client" case the lookup
        // documents as a miss.
        unsafe { runtime.lookup_security_context(std::ptr::null()) }.is_none(),
        "no manager, no lookup"
    );

    runtime
        .create_security_context_manager(&display)
        .expect("manager");
    assert!(
        runtime.create_security_context_manager(&display).is_err(),
        "a second manager is refused"
    );

    assert!(
        // SAFETY: as above.
        unsafe { runtime.lookup_security_context(std::ptr::null()) }.is_none(),
        "a null client names no live context"
    );
}

/// A client creates and commits a `wp_security_context_v1`; the metadata it
/// attached reaches the handler, and the delivered [`SecurityContext`] owns its
/// strings.
#[test]
fn a_client_commits_a_security_context() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_security_context_manager(&display)
        .expect("manager");

    let socket = display.add_socket_auto().expect("socket");
    let mut app = App {
        client: Some(common::client::spawn_security_context(&socket)),
        contexts: Vec::new(),
    };

    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(app.contexts.len(), 1, "one commit reached the handler");
    let state = app.contexts[0].state();
    assert_eq!(state.sandbox_engine(), Some("org.wlr.test"));
    assert_eq!(state.app_id(), Some("org.wlr.test.app"));
    assert_eq!(state.instance_id(), Some("instance-1"));

    // A live `wl_client` lookup (`Some`) is infeasible here without new
    // plumbing: `spawn_security_context` returns only `JoinHandle<()>` and the
    // server never exposes the sandbox connection's `wl_client` to the test,
    // and `lookup_security_context`'s contract requires a null-or-live
    // pointer (a disconnected/dangling pointer would be dereferenced by
    // wlroots, so it cannot be passed safely). The safe post-run check is
    // that a null lookup still misses with the manager alive after the
    // sandbox client has disconnected.
    assert!(
        // SAFETY: null is the documented always-safe miss.
        unsafe { runtime.lookup_security_context(std::ptr::null()) }.is_none(),
        "after the sandbox client disconnects, a null client still names no live context"
    );

    // The recorded context owns its strings: it stays readable after the client
    // is gone and the compositor is torn down. Drop in declaration order's
    // reverse — runtime (which unlinks its backend listeners), then the
    // backend, then the display the backend borrows.
    let kept = app.contexts.pop().expect("the recorded context");
    drop(app);
    drop(runtime);
    drop(backend);
    drop(display);
    assert_eq!(kept.sandbox_engine(), Some("org.wlr.test"));
    assert_eq!(kept.app_id(), Some("org.wlr.test.app"));
    assert_eq!(kept.instance_id(), Some("instance-1"));
}

/// A plain surface (and a dangling id) is not a session-lock surface: the
/// downcast misses rather than dereferencing a role the surface never had.
#[test]
fn lock_surface_lookup_misses_without_the_role() {
    let _serial = common::headless_guard();
    common::headless_env();
    let _display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    assert!(
        runtime
            .lock_surface(SurfaceId::dangling_for_test())
            .is_none(),
        "a dangling surface id names no lock surface"
    );
}

/// The `wlr_fixes` global creates once and refuses a second create.
#[test]
fn fixes_global_creates_once() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    runtime.create_fixes(&display, 1).expect("fixes global");
    assert!(
        runtime.create_fixes(&display, 1).is_err(),
        "a second fixes global is refused"
    );
}
