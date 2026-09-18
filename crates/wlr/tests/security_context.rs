//! `wp_security_context_v1` and the session-lock surface completion.
//!
//! The manager create/lookup contract is exercised without a client. The commit
//! path is client-driven: a real `wayland-client` connection binds
//! `wp_security_context_manager_v1`, creates a listener, attaches the metadata
//! and commits — and the server-side handler records the [`SecurityContext`] it
//! was handed. That value owns its strings, so it is read back after the client
//! thread and the display are gone. The commit also carries the
//! committing-client attribution, and the handler resolves a live lookup on it
//! while the client is still connected.

mod common;

use std::thread::JoinHandle;

use wlr::{
    Backend, Display, Error, OutputId, Runtime, SecurityContext, SurfaceId, ToplevelHandler, Until,
};

/// Records every committed security context the server-side handler observes,
/// and stops the run once the driving client thread ends.
#[derive(Default)]
struct App {
    client: Option<JoinHandle<()>>,
    runtime: Option<Runtime>,
    contexts: Vec<SecurityContext>,
    /// Whether the delivered context carried a committing client.
    committer_present: Option<bool>,
    /// What a live `lookup_security_context` on that client returned, resolved
    /// inside the commit handler while the client was still connected.
    live_lookup: Option<Option<SecurityContext>>,
}

impl wlr::OutputHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}

impl ToplevelHandler for App {
    fn security_context_committed(&mut self, context: &SecurityContext) {
        self.committer_present = Some(context.committing_client().is_some());
        // The committing client is live for exactly this call, so this is
        // the one place a lookup on it is sound.
        self.live_lookup = Some(self.runtime.as_ref().and_then(|runtime| {
            context
                .committing_client()
                .and_then(|client| unsafe { runtime.lookup_security_context(client as *const _) })
        }));
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
///
/// The refusal is [`Error::Operation`], not [`Error::Create`]: the crate
/// rejects the second create before calling C. The `Create` (wlroots-returned-
/// null) arm stays OOM-only — a failed allocation inside wlroots, which no
/// test can induce on purpose.
#[test]
fn security_context_manager_creates_once_and_lookup_misses() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    assert_eq!(
        // SAFETY: a null client is the explicit "no client" case the lookup
        // documents as a miss.
        unsafe { runtime.lookup_security_context(std::ptr::null()) },
        None,
        "no manager, no lookup"
    );

    runtime
        .create_security_context_manager(&display)
        .expect("manager");
    assert!(
        matches!(
            runtime.create_security_context_manager(&display),
            Err(Error::Operation(_))
        ),
        "a second manager is refused before C is called"
    );

    assert_eq!(
        // SAFETY: as above.
        unsafe { runtime.lookup_security_context(std::ptr::null()) },
        None,
        "a null client names no live context"
    );
}

/// The manager dies with its display: the destroy watch clears the stored
/// pointer, so a later lookup misses instead of dereferencing freed memory —
/// mirroring the foreign-toplevel manager's death test.
#[test]
fn dropping_the_display_clears_the_security_context_lookup() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");
    runtime
        .create_security_context_manager(&display)
        .expect("manager");

    drop(display);

    assert_eq!(
        // SAFETY: null is the documented always-safe miss; the manager is
        // gone, so the alive-flag short-circuit returns before any pointer
        // is touched.
        unsafe { runtime.lookup_security_context(std::ptr::null()) },
        None,
        "after display teardown the lookup misses instead of touching the freed manager"
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
        runtime: Some(runtime.clone()),
        contexts: Vec::new(),
        committer_present: None,
        live_lookup: None,
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

    // The commit carried its attribution: wlroots' commit-event
    // `parent_client` (the sandbox-engine connection) reached the delivered
    // value as an opaque pointer.
    assert_eq!(
        app.committer_present,
        Some(true),
        "the committed context names its committing client"
    );

    // A live lookup on the committing client misses: wlroots attaches the
    // committed state to the connections the sandbox *accepts* through its
    // listen socket, not to the engine connection that committed it. This
    // doubles as the live-client-no-context negative: a connected, live
    // client with no attached context resolves to `None`, not to anything
    // else's metadata.
    assert!(
        matches!(&app.live_lookup, Some(None)),
        "a live client with no attached context misses"
    );

    // A live `wl_client` lookup that returns `Some` is infeasible without new
    // plumbing, and deliberately so. The accepted connection's server-side
    // `wl_client` is observable only through a libwayland global filter
    // (`wl_display_set_global_filter`): `Display` exposes no such hook — its
    // surface is `new` / `add_socket_auto` / `flush_clients` / `event_loop` /
    // `dispatch` — and the raw display pointer it would take is `pub(crate)`,
    // so a test cannot install one either. `spawn_security_context` returns
    // only `JoinHandle<()>` and the server never exposes the sandbox
    // connection's `wl_client` anywhere else, and `lookup_security_context`'s
    // contract requires a null-or-live pointer (a disconnected/dangling
    // pointer would be dereferenced by wlroots, so it cannot be passed
    // safely). The safe post-run check is that a null lookup still misses
    // with the manager alive after the sandbox client has disconnected —
    // matched explicitly as `None`, since the lookup returns `Option` and
    // has no error variant to match instead.
    assert_eq!(
        // SAFETY: null is the documented always-safe miss.
        unsafe { runtime.lookup_security_context(std::ptr::null()) },
        None,
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

/// One observation of a live lock surface, recorded inside the commit handler
/// (surface ids are only meaningful for the run that announced them, so every
/// resolution here happens while the run is on the stack).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LockObservation {
    /// The surface the observation was taken on.
    id: SurfaceId,
    /// [`wlr::LockSurface::configured_size`], through both resolution paths.
    configured: Option<(u32, u32)>,
    /// [`wlr::LockSurfaceState::configure_serial`].
    serial: Option<u32>,
    /// [`wlr::LockSurface::output`], resolved to the registered output id.
    output: Option<OutputId>,
    /// Whether [`Runtime::lock_surface`] resolved the same surface.
    via_runtime: bool,
}

/// A real `ext_session_lock_v1` locker covers the headless output: the lock
/// surface resolves through both [`Runtime::lock_surface`] and
/// [`wlr::Surface::lock_surface`], its configured size matches the configure
/// the client observed, and its output resolves to the registered output.
#[derive(Default)]
struct LockApp {
    runtime: Option<Runtime>,
    client: Option<JoinHandle<common::client::SessionLockEvents>>,
    output: Option<OutputId>,
    output_ready: Option<bool>,
    observations: Vec<LockObservation>,
}

impl wlr::OutputHandler for LockApp {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        self.output = Some(output.id());
        // Enable at the preferred mode and init render: the enable gives the
        // lock-surface configure a real size, and `init_output` puts the
        // output in the layout — which is what exposes its `wl_output` global
        // to clients, without which the locker could not name the output.
        // Both report Results, so their outcomes are recorded, never panicked
        // on: a handler must not unwind through C.
        let enabled_ok = output.enable_with_preferred_mode().is_ok();
        let init_ok = self
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.init_output(output).is_ok());
        self.output_ready = Some(enabled_ok && init_ok);
    }
}

impl wlr::SeatHandler for LockApp {}
impl wlr::FdHandler for LockApp {}

impl ToplevelHandler for LockApp {
    fn surface_committed(&mut self, surface: &wlr::Surface<'_>) {
        // The `Surface::lock_surface` path: only a lock surface resolves.
        let Some(lock) = surface.lock_surface() else {
            return;
        };
        let state = lock.state();
        self.observations.push(LockObservation {
            id: surface.id(),
            configured: lock.configured_size(),
            serial: state.map(|s| s.configure_serial()),
            output: lock.output().map(|o| o.id()),
            // The `Runtime::lock_surface` path, on the same surface.
            via_runtime: self
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.lock_surface(surface.id()).is_some()),
        });
    }
}

impl wlr::LoopHandler for LockApp {
    fn should_stop(&mut self) -> bool {
        // The client's closing round-trip is flushed through the server, so a
        // finished client means every lock request — and its commits — has
        // been dispatched. Observations are required too: without them the
        // test would pass vacuously on a run that never saw the surface.
        self.client.as_ref().is_some_and(|h| h.is_finished()) && !self.observations.is_empty()
    }
}

#[test]
fn a_locker_covers_the_headless_output() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_session_lock_manager(&display)
        .expect("session-lock manager");

    let socket = display.add_socket_auto().expect("socket");
    let mut app = LockApp {
        runtime: Some(runtime.clone()),
        client: Some(common::client::spawn_session_lock(&socket)),
        output: None,
        output_ready: None,
        observations: Vec::new(),
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

    let output = app.output.expect("the headless output was announced");
    assert_eq!(
        app.output_ready,
        Some(true),
        "the headless output enabled and initialised, exposing its wl_output global"
    );
    let (serial, width, height) = events.configure.expect("the client saw a configure");
    assert_ne!(
        (width, height),
        (0, 0),
        "the output was enabled, so the configure is real"
    );
    assert_eq!(
        events.acked_serial,
        Some(serial),
        "the driver acked the configure it observed"
    );
    assert!(
        events.saw_locked,
        "every output is covered by a mapped lock surface, so the server sent `locked`"
    );
    assert!(
        !events.saw_finished,
        "the locker never unlocked during the run"
    );

    // The surface table is cleared when the run returns, so the observations
    // recorded inside the handler are the assertions: the last one is the
    // mapped commit, whose `current` carries the acked configure.
    let last = app.observations.last().expect("a lock surface committed");
    assert_eq!(
        last.configured,
        Some((width, height)),
        "the server-side configured size matches the client's configure"
    );
    assert_eq!(
        last.serial,
        Some(serial),
        "the server-side serial matches the acked configure"
    );
    assert_eq!(
        last.output,
        Some(output),
        "the lock surface resolves to the registered output"
    );
    assert!(
        last.via_runtime,
        "Runtime::lock_surface resolved the same surface as Surface::lock_surface"
    );
    assert!(
        app.observations.iter().all(|o| o.id == last.id),
        "every lock observation named the same surface"
    );
}

/// A live plain toplevel is not a lock surface: both resolution paths miss on
/// a tracked, mapped, non-lock surface — not just on a dangling id.
#[derive(Default)]
struct PlainApp {
    runtime: Option<Runtime>,
    client: Option<JoinHandle<common::client::ClientEvents>>,
    saw_surface: bool,
    lock_miss: Option<bool>,
}

impl wlr::OutputHandler for PlainApp {}
impl wlr::SeatHandler for PlainApp {}
impl wlr::FdHandler for PlainApp {}

impl ToplevelHandler for PlainApp {
    fn surface_committed(&mut self, surface: &wlr::Surface<'_>) {
        self.saw_surface = true;
        let miss = surface.lock_surface().is_none()
            && self
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.lock_surface(surface.id()).is_none());
        self.lock_miss = Some(self.lock_miss.unwrap_or(true) && miss);
    }
}

impl wlr::LoopHandler for PlainApp {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished()) && self.saw_surface
    }
}

#[test]
fn lock_surface_lookup_misses_on_a_live_plain_surface() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");

    let socket = display.add_socket_auto().expect("socket");
    let mut app = PlainApp {
        runtime: Some(runtime.clone()),
        client: Some(common::client::spawn_mapped(&socket)),
        saw_surface: false,
        lock_miss: None,
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
        app.lock_miss,
        Some(true),
        "a mapped plain toplevel resolves through neither lock-surface path"
    );
}
