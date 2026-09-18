//! Sub-surface role snapshots, against a real headless compositor.
//!
//! Three positive paths, one destroy-order witness and one invisibility
//! proof. A real Wayland client creates an `xdg_toplevel` parent, a child
//! `wl_surface` and a `wl_subsurface` on it; the server must observe the
//! child through its generic `new_subsurface` event, and read the child's
//! parent id and committed parent-relative position through the transient
//! accessors — both at announce time and on a later child commit. The
//! destroy-order test destroys the **parent** surface while keeping the child
//! alive and proves the accessors miss after wlroots has freed the role
//! object — the use-after-free the snapshot-only API exists to prevent. The
//! roleless-parent test parents a child to a surface with no role at all and
//! proves the server observes nothing: with no announce site the parent is
//! never installed, so there is no listener to announce the child through.
//!
//! The deterministic roleless-surface miss lives in `subsurface.rs`'s unit test,
//! which needs no client.

mod common;

use std::thread::JoinHandle;

use wlr::{Backend, Display, Runtime, SubsurfaceParentState, SurfaceId, Until};

/// One `new_subsurface` observation: the announced pair plus everything read
/// through the transient accessors while the role was live.
///
/// Grouped rather than kept as parallel `Vec`s so a length-mismatch failure
/// names the observation, not an index.
struct NewSubsurface {
    parent: SurfaceId,
    child: SurfaceId,
    /// `subsurface_parent_id()` result read during `new_subsurface`.
    parent_id: Option<SurfaceId>,
    /// `subsurface_parent_state()` result read during `new_subsurface`.
    parent_state: Option<SubsurfaceParentState>,
    /// `runtime.toplevel_of(child)` on the live child: must miss, since the
    /// child carries the sub-surface role rather than the toplevel one.
    toplevel_of_child_missed: bool,
    /// `runtime.popup_of(parent)` on the live toplevel parent: must miss,
    /// since the parent is a toplevel rather than a popup.
    popup_of_parent_missed: bool,
    /// `child.as_toplevel()` on the live child surface: must miss for the
    /// same role reason as `toplevel_of`.
    child_as_toplevel_missed: bool,
}

/// One observed child commit: the committing child plus the accessor reads
/// from the commit handler, which run against the live role (or miss after
/// the parent destroyed it).
struct ChildCommit {
    child: SurfaceId,
    /// `subsurface_parent_id()` read during the child commit.
    parent_id: Option<SurfaceId>,
    /// `subsurface_parent_state()` read during the child commit.
    parent_state: Option<SubsurfaceParentState>,
}

#[derive(Default)]
struct App {
    /// A clone of the runtime, so the handlers can resolve a child handle and
    /// exercise the accessors from inside a handler.
    runtime: Option<Runtime>,
    toplevels: usize,
    /// Per-`new_subsurface` observations, in order.
    subsurface: Vec<NewSubsurface>,
    /// Per-child-commit observations, in order.
    commits: Vec<ChildCommit>,
    /// Whether the parent toplevel surface's destroy was observed, so the
    /// destroy-order test can prove the child commit came after it.
    parent_destroyed: bool,
    /// The client thread, owned here so [`LoopHandler::should_stop`] can end the
    /// single `Until::Stop` run once the client is done.
    client: Option<JoinHandle<common::client::ClientEvents>>,
}

impl App {
    fn is_child(&self, id: SurfaceId) -> bool {
        self.subsurface.iter().any(|o| o.child == id)
    }

    fn first_parent(&self) -> Option<SurfaceId> {
        self.subsurface.first().map(|o| o.parent)
    }
}

impl wlr::OutputHandler for App {}
impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, _t: &wlr::Toplevel<'_>) {
        self.toplevels += 1;
    }

    fn new_subsurface(&mut self, parent: SurfaceId, child: SurfaceId) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        let Some(surface) = runtime.surface(child) else {
            eprintln!("new_subsurface for an untracked child: parent={parent:?} child={child:?}");
            return;
        };
        self.subsurface.push(NewSubsurface {
            parent,
            child,
            parent_id: surface.subsurface_parent_id(),
            parent_state: surface.subsurface_parent_state(),
            // Wrong-role downcasts against live tracked surfaces: the child is a
            // sub-surface, not a toplevel, and the parent is a toplevel, not a
            // popup — both must miss rather than dereference the wrong role.
            toplevel_of_child_missed: runtime.toplevel_of(child).is_none(),
            popup_of_parent_missed: runtime.popup_of(parent).is_none(),
            child_as_toplevel_missed: surface.as_toplevel().is_none(),
        });
    }

    fn surface_committed(&mut self, surface: &wlr::Surface<'_>) {
        // Only the child surfaces are interesting here; the parent's own
        // commits must not be mistaken for a sub-surface observation.
        if self.is_child(surface.id()) {
            self.commits.push(ChildCommit {
                child: surface.id(),
                parent_id: surface.subsurface_parent_id(),
                parent_state: surface.subsurface_parent_state(),
            });
        }
    }

    fn surface_destroyed(&mut self, id: SurfaceId) {
        if self.first_parent().is_some_and(|p| p == id) {
            self.parent_destroyed = true;
        }
    }
}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

/// Read the snapshot's accessors, so the assertions compare plain `(x, y)`
/// tuples rather than reaching into a type whose fields are private.
fn position(state: &SubsurfaceParentState) -> (i32, i32) {
    (state.x(), state.y())
}

/// A real client creates an xdg-toplevel parent, then a child surface and a
/// `wl_subsurface` at `(10, 20)` on it, and disconnects. The server must observe
/// exactly one `new_subsurface` naming that parent and read the child's parent
/// id and committed position through the transient accessors.
#[test]
fn a_real_client_subsurface_is_observed_and_reads_its_parent() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        runtime: Some(runtime.clone()),
        client: Some(common::client::spawn_subsurface(&socket)),
        ..App::default()
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
        events.configure_events >= 1,
        "the parent toplevel's initial configure must arrive and be acked"
    );
    assert_eq!(
        events.acked_configures, events.configure_events,
        "every configure the client saw must have been acked"
    );
    assert_eq!(
        app.toplevels, 1,
        "the parent toplevel should be announced before its subsurface"
    );
    assert_eq!(
        app.subsurface.len(),
        1,
        "the server should observe exactly one new_subsurface from the client"
    );
    let obs = &app.subsurface[0];
    assert_eq!(
        obs.parent_id,
        Some(obs.parent),
        "the child's subsurface_parent_id names the toplevel parent"
    );
    assert_eq!(
        obs.parent_state.as_ref().map(position),
        Some((10, 20)),
        "the position the client set is what the parent committed"
    );
    assert!(
        obs.toplevel_of_child_missed,
        "toplevel_of on the live sub-surface child must miss (wrong role)"
    );
    assert!(
        obs.popup_of_parent_missed,
        "popup_of on the live toplevel parent must miss (wrong role)"
    );
    assert!(
        obs.child_as_toplevel_missed,
        "Surface::as_toplevel on the live sub-surface child must miss"
    );
    assert!(
        app.commits.is_empty(),
        "the bufferless child's only commit ran before the role was announced, \
         so no child commit reaches surface_committed on this path"
    );
}

/// A real client maps a parent toplevel and a child sub-surface at `(10, 20)`,
/// then commits the child once more after the role was announced. That
/// post-announce commit must reach `surface_committed` with the role live, so
/// the commit-time accessor reads resolve the parent and its position —
/// gutting the commit-handler read fails this test.
#[test]
fn a_post_announce_child_commit_resolves_its_parent() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        runtime: Some(runtime.clone()),
        client: Some(common::client::spawn_subsurface_mapped(&socket)),
        ..App::default()
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
        events.configure_events >= 1,
        "the parent toplevel's initial configure must arrive and be acked"
    );
    assert_eq!(
        events.acked_configures, events.configure_events,
        "every configure the client saw must have been acked"
    );
    assert_eq!(app.toplevels, 1, "the parent toplevel should be announced");
    assert_eq!(
        app.subsurface.len(),
        1,
        "the server should observe exactly one new_subsurface from the client"
    );
    let obs = &app.subsurface[0];
    assert_eq!(
        obs.parent_id,
        Some(obs.parent),
        "while the role is alive the parent id resolves at announce time"
    );
    // The pre-announce child commit ran before the child was installed, so
    // the single commit seen here is the post-announce one the driver makes.
    assert_eq!(
        app.commits.len(),
        1,
        "exactly the post-announce child commit reaches surface_committed"
    );
    let commit = &app.commits[0];
    assert_eq!(
        commit.child, obs.child,
        "the observed commit is the announced child's"
    );
    assert_eq!(
        commit.parent_id,
        Some(obs.parent),
        "the commit-time subsurface_parent_id names the toplevel parent"
    );
    assert_eq!(
        commit.parent_state.as_ref().map(position),
        Some((10, 20)),
        "the commit-time position is what the parent applied"
    );
}

/// A real client creates the same parent + child sub-surface, then destroys the
/// **parent** surface while keeping the child, and commits the child once more.
///
/// wlroots frees the `wlr_subsurface` from the parent's destroy signal while the
/// child `wlr_surface` survives, so the post-destroy child commit is the point
/// at which a stored `wlr_subsurface *` would dangle. Both accessors must miss
/// there rather than read freed memory.
#[test]
fn accessors_miss_after_the_parent_surface_is_destroyed() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        runtime: Some(runtime.clone()),
        client: Some(common::client::spawn_subsurface_then_destroy_parent(
            &socket,
        )),
        ..App::default()
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
        events.configure_events >= 1,
        "the parent toplevel's initial configure must arrive and be acked"
    );
    assert_eq!(
        events.acked_configures, events.configure_events,
        "every configure the client saw must have been acked"
    );
    assert_eq!(
        app.subsurface.len(),
        1,
        "the server should observe the sub-surface before the parent dies"
    );
    let obs = &app.subsurface[0];
    assert_eq!(
        obs.parent_id,
        Some(obs.parent),
        "while the role is alive the parent id resolves"
    );
    assert!(
        app.parent_destroyed,
        "the parent surface destroy must have been observed"
    );
    // The child is only tracked when `new_subsurface` announces the role, which
    // is *after* the client's first child commit, so the single commit seen here
    // is the one the client makes after destroying the parent.
    assert_eq!(
        app.commits.len(),
        1,
        "the only observed child commit is the post-parent-destroy one"
    );
    assert_eq!(
        app.commits[0].child, obs.child,
        "that commit is the announced child's"
    );
    assert_eq!(
        app.commits[0].parent_id, None,
        "after the parent is destroyed the role is gone and the accessor must miss, \
         not dereference the freed wlr_subsurface"
    );
    assert_eq!(
        app.commits[0].parent_state, None,
        "and the committed position accessor must miss too"
    );
}

/// A real client parents a child sub-surface to a roleless plain surface — no
/// xdg role, so no announce site ever installs the parent — and disconnects.
/// The server must observe nothing at all: no toplevel, no `new_subsurface`,
/// no child commit, no configure.
///
/// This is the observable half of the untracked-parent case. The other half —
/// `subsurface_parent_id() == None` beside a `Some` position read through a
/// handler — is unexpressible on purpose: the server hands a handler a child
/// only once the parent's `new_subsurface` listener announced it, and that
/// listener is installed together with the parent's surface-id addon. An
/// observed child therefore always descends from an addon-carrying parent;
/// an addon-less parent's whole subtree stays dark, which is what this test
/// pins.
#[test]
fn a_subsurface_on_a_roleless_parent_is_never_observed() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    // No xdg-shell, no seat: the world is a bare compositor plus the
    // subcompositor `init_graphics` creates, so the roleless parent has no
    // announce site at all.
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        runtime: Some(runtime.clone()),
        client: Some(common::client::spawn_subsurface_on_roleless_parent(&socket)),
        ..App::default()
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

    assert_eq!(
        app.toplevels, 0,
        "a roleless surface is never announced as a toplevel"
    );
    assert!(
        app.subsurface.is_empty(),
        "with no installed parent there is no new_subsurface listener, \
         so the child is never announced"
    );
    assert!(
        app.commits.is_empty(),
        "the child is never installed, so its commits never reach surface_committed"
    );
    assert_eq!(
        events.configure_events, 0,
        "nothing took an xdg role, so no configure exists"
    );
    assert_eq!(events.acked_configures, 0, "and nothing was acked");
}
