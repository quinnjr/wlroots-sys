//! Sub-surface role handles, against a real headless compositor.
//!
//! A real Wayland client creates an `xdg_toplevel` parent, then a child
//! `wl_surface` and a `wl_subsurface` on top of it. The server must observe the
//! child through its generic `new_subsurface` event, and the child's
//! [`Surface`] must downcast to a [`Subsurface`] whose parent id and committed
//! parent-relative position read back through the real wlroots objects.
//!
//! The deterministic miss contract — a roleless surface does not downcast, and
//! a dangling id never resolves — lives in `subsurface.rs`'s own unit tests,
//! which do not need a client.

mod common;

use std::thread::JoinHandle;

use wlr::{Backend, Display, Runtime, SurfaceId, Until};

#[derive(Default)]
struct App {
    /// A clone of the runtime, so `new_subsurface` can resolve the child handle
    /// and exercise the downcast from inside the handler.
    runtime: Option<Runtime>,
    toplevels: usize,
    /// `(parent, child)` pairs the server observed, in order.
    subsurface_pairs: Vec<(SurfaceId, SurfaceId)>,
    /// The parent id [`Subsurface::parent_surface_id`] reported per child.
    parent_ids: Vec<Option<SurfaceId>>,
    /// The committed parent-relative offsets [`Subsurface::parent_state`]
    /// reported per child.
    parent_states: Vec<wlr::SubsurfaceParentState>,
    /// The client thread, owned here so [`LoopHandler::should_stop`] can end the
    /// single `Until::Stop` run once the client is done. A `SurfaceId` (and the
    /// subsurface role behind it) is only good for the run that announced the
    /// surface, so the listeners must stay linked across announce → destroy.
    client: Option<JoinHandle<common::client::ClientEvents>>,
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
        let Some(subsurface) = surface.as_subsurface() else {
            return;
        };
        self.parent_ids.push(subsurface.parent_surface_id());
        self.parent_states.push(subsurface.parent_state());
    }
}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

/// A real client creates an xdg-toplevel parent, then a child surface and a
/// `wl_subsurface` on it, and finally disconnects. The server must observe
/// exactly one `new_subsurface` naming that parent, and the child must downcast
/// to a [`wlr::Subsurface`] whose `parent_surface_id` names the toplevel.
#[test]
fn a_real_client_subsurface_is_observed_and_downcasts() {
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
    let (parent, _child) = app.subsurface_pairs[0];
    assert_eq!(
        app.parent_ids,
        vec![Some(parent)],
        "the child downcasts to a Subsurface whose parent id names the toplevel"
    );
    assert_eq!(
        app.parent_states.len(),
        1,
        "the parent-relative position is readable through the live subsurface"
    );
}
