//! A real Wayland client for the client-driven integration-test harness.
//!
//! The six-leg standard could only exercise the server through wlroots' own
//! in-process seams, so nothing ever spoke the wire protocol back at the
//! display: commit/ack sequencing, configure round-trips and client-driven
//! unmap/destroy were unreachable. This module connects a genuine
//! `wayland-client` connection on its own thread, drives a caller-supplied
//! closure against it, and blocks until the server has seen the resulting
//! requests.
//!
//! Call [`crate::common::isolated_runtime_dir`] before `Display::new` — the
//! server's socket and the client both resolve `WAYLAND_DISPLAY` against
//! `XDG_RUNTIME_DIR`, and the client thread only sets the former.

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// The globals a driven client has bound.
///
/// The two `Option`s are filled by [`spawn`] before the `drive` closure runs;
/// a closure can rely on both being `Some` and calling e.g.
/// [`create_toplevel`](ClientState::create_toplevel) directly. They are public
/// so a future leg can bind additional globals the same way.
pub struct ClientState {
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub wm_base: Option<xdg_wm_base::XdgWmBase>,
}

impl ClientState {
    /// Create and commit an unmapped xdg-shell toplevel.
    ///
    /// No buffer is attached: the server's `new_toplevel` fires when the
    /// `xdg_toplevel` role is created, so mapping is not needed to observe it.
    /// The commit is still required — xdg-shell clients role-then-commit, and a
    /// later leg that wants a configure must be on the commit path.
    pub fn create_toplevel(&mut self, qh: &QueueHandle<Self>) {
        let compositor = self.compositor.as_ref().expect("wl_compositor not bound");
        let wm_base = self.wm_base.as_ref().expect("xdg_wm_base not bound");
        let surface = compositor.create_surface(qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, ());
        let _toplevel = xdg_surface.get_toplevel(qh, ());
        surface.commit();
    }
}

/// Connect a client on its own thread, run `drive`, then round-trip.
///
/// `socket` is the name [`wlr::Display::add_socket_auto`] returned; it is
/// assigned to `WAYLAND_DISPLAY`. `XDG_RUNTIME_DIR` is *not* set here on
/// purpose — the caller set it via
/// [`crate::common::isolated_runtime_dir`] before creating the display, and
/// re-reading the same process-global here is what makes both ends agree on
/// where the socket lives.
///
/// The thread ends with a blocking round-trip: it flushes everything `drive`
/// queued and then waits for the server's sync reply, so `join` returns only
/// once the server has dispatched those requests. Without it a fast server loop
/// could reach the caller's assertion while the last `commit` was still in the
/// client's outgoing buffer.
pub fn spawn(
    socket: &str,
    drive: impl FnOnce(&mut ClientState, &QueueHandle<ClientState>) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let socket = socket.to_owned();
    std::thread::spawn(move || {
        // SAFETY: `set_var` writes process-global state; this thread touches no
        // environment-dependent libwayland path before the write, and the
        // caller has already set `XDG_RUNTIME_DIR` and stopped mutating it. The
        // server's own `add_socket_auto` read its value on the main thread
        // before this thread existed.
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", &socket);
        }

        let conn = Connection::connect_to_env().expect("connect to WAYLAND_DISPLAY");
        let (globals, mut queue) =
            registry_queue_init::<ClientState>(&conn).expect("registry queue init");
        let qh = queue.handle();

        let mut state = ClientState {
            compositor: None,
            wm_base: None,
        };
        // `registry_queue_init` buffers the initial globals rather than
        // forwarding them to a handler, so the standard `GlobalList::bind` is
        // the way to bind them; binding here also guarantees both are present
        // before `drive` runs.
        state.compositor = Some(globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor"));
        state.wm_base = Some(globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base"));

        drive(&mut state, &qh);

        // Deliberately no `conn.flush()` here before the round-trip. A
        // round-trip flushes everything buffered *and* its own sync request in
        // one write, so the server reads the toplevel requests and the sync in
        // the same dispatch turn and answers the sync in that turn. Flushing
        // first instead sends the toplevel requests on their own, letting a
        // server driven by `Until::Turns` observe the toplevel and stop before
        // the sync ever arrives — which deadlocks this blocking call and, with
        // it, the caller's `join`. That is not theoretical: it reproduced
        // roughly one run in twenty-five before the flush was dropped.
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the requests");
    })
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientState {
    /// Present only to satisfy `registry_queue_init`'s bound. The initial
    /// globals are consumed through `GlobalList::bind` in [`spawn`]; this fires
    /// only for globals added after the initial round-trip, which the harness
    /// does not exercise.
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

impl Dispatch<wl_compositor::WlCompositor, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for ClientState {
    fn event(
        _state: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The server pings to test liveness and kills a client that does not
        // answer, so this must be answered even though the seed test ignores
        // the rest.
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for ClientState {
    fn event(
        _state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // xdg-shell requires every configure be acked; a later leg that waits
        // for a mapped window depends on it.
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &xdg_toplevel::XdgToplevel,
        _event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
