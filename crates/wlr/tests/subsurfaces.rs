//! Sub-surface role snapshots, against a real headless compositor.
//!
//! Two positive paths and one destroy-order witness. A real Wayland client
//! creates an `xdg_toplevel` parent, a child `wl_surface` and a `wl_subsurface`
//! on it; the server must observe the child through its generic
//! `new_subsurface` event, and read the child's parent id and committed
//! parent-relative position through the transient accessors. The destroy-order
//! test destroys the **parent** surface while keeping the child alive and proves
//! the accessors miss after wlroots has freed the role object — the
//! use-after-free the snapshot-only API exists to prevent.
//!
//! The deterministic roleless-surface miss lives in `subsurface.rs`'s unit test,
//! which needs no client.

mod common;

use std::thread::JoinHandle;

use wlr::{Backend, Display, Runtime, SubsurfaceParentState, SurfaceId, Until};

#[derive(Default)]
struct App {
    /// A clone of the runtime, so the handlers can resolve a child handle and
    /// exercise the accessors from inside a handler.
    runtime: Option<Runtime>,
    toplevels: usize,
    /// `(parent, child)` pairs the server observed, in order.
    subsurface_pairs: Vec<(SurfaceId, SurfaceId)>,
    /// `subsurface_parent_id()` result read during `new_subsurface`.
    parent_ids: Vec<Option<SurfaceId>>,
    /// `subsurface_parent_state()` result read during `new_subsurface`.
    parent_states: Vec<Option<SubsurfaceParentState>>,
    /// Child surfaces observed through `surface_committed`, in order.
    committed_children: Vec<SurfaceId>,
    /// `subsurface_parent_id()` read during each child commit.
    committed_parent_ids: Vec<Option<SurfaceId>>,
    /// `subsurface_parent_state()` read during each child commit.
    committed_parent_states: Vec<Option<SubsurfaceParentState>>,
    /// Whether the parent toplevel surface's destroy was observed, so the
    /// destroy-order test can prove the child commit came after it.
    parent_destroyed: bool,
    /// The client thread, owned here so [`LoopHandler::should_stop`] can end the
    /// single `Until::Stop` run once the client is done.
    client: Option<JoinHandle<common::client::ClientEvents>>,
}

impl App {
    fn is_child(&self, id: SurfaceId) -> bool {
        self.subsurface_pairs.iter().any(|(_, child)| *child == id)
    }
}

impl wlr::OutputHandler for App {}
impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, _t: &wlr::Toplevel<'_>) {
        self.toplevels += 1;
    }

    fn new_subsurface(&mut self, parent: SurfaceId, child: SurfaceId) {
        self.subsurface_pairs.push((parent, child));
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        let Some(surface) = runtime.surface(child) else {
            return;
        };
        self.parent_ids.push(surface.subsurface_parent_id());
        self.parent_states.push(surface.subsurface_parent_state());
    }

    fn surface_committed(&mut self, surface: &wlr::Surface<'_>) {
        // Only the child surfaces are interesting here; the parent's own
        // commits must not be mistaken for a sub-surface observation.
        if self.is_child(surface.id()) {
            self.committed_children.push(surface.id());
            self.committed_parent_ids
                .push(surface.subsurface_parent_id());
            self.committed_parent_states
                .push(surface.subsurface_parent_state());
        }
    }

    fn surface_destroyed(&mut self, id: SurfaceId) {
        if self.subsurface_pairs.first().is_some_and(|(p, _)| *p == id) {
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

    let _events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(
        app.toplevels, 1,
        "the parent toplevel should be announced before its subsurface"
    );
    assert_eq!(
        app.subsurface_pairs.len(),
        1,
        "the server should observe exactly one new_subsurface from the client"
    );
    let (parent, child) = app.subsurface_pairs[0];
    assert_eq!(
        app.parent_ids,
        vec![Some(parent)],
        "the child's subsurface_parent_id names the toplevel parent"
    );
    assert_eq!(
        app.parent_states.len(),
        1,
        "the child's committed parent-relative position is readable"
    );
    assert_eq!(
        app.parent_states[0].as_ref().map(position),
        Some((10, 20)),
        "the position the client set is what the parent committed"
    );
    let _ = child;
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

    let _events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(
        app.subsurface_pairs.len(),
        1,
        "the server should observe the sub-surface before the parent dies"
    );
    let (parent, child) = app.subsurface_pairs[0];
    assert_eq!(
        app.parent_ids,
        vec![Some(parent)],
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
        app.committed_children,
        vec![child],
        "the only observed child commit is the post-parent-destroy one"
    );
    assert_eq!(
        app.committed_parent_ids,
        vec![None],
        "after the parent is destroyed the role is gone and the accessor must miss, \
         not dereference the freed wlr_subsurface"
    );
    assert_eq!(
        app.committed_parent_states,
        vec![None],
        "and the committed position accessor must miss too"
    );
}
