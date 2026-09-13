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
//! server binds its socket *under* `XDG_RUNTIME_DIR`, and [`spawn`] resolves
//! the returned socket name against the same directory itself. It never
//! touches `WAYLAND_DISPLAY`: `wayland-client` 0.31 has no name-taking
//! connect, so the path is built on the caller thread and handed to
//! [`Connection::from_socket`], keeping `setenv`/`getenv` out of the spawned
//! thread entirely.

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// What the client observed while it ran, handed back by [`spawn`].
///
/// The seed connection is owned by the spawned thread, so a test cannot read
/// the configure/ack flags off the [`ClientState`] it never sees. These are
/// copied out after the round-trips and returned through the `JoinHandle`, so
/// the test can assert the wire exchange actually happened rather than only
/// that the server announced a toplevel.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClientEvents {
    /// `xdg_surface.configure` events dispatched to this client.
    pub configure_events: u32,
    /// `xdg_surface.ack_configure` requests sent in response to those.
    pub acked_configures: u32,
}

/// The globals a driven client has bound.
///
/// The two `Option`s are filled by [`spawn`] before the `drive` closure runs;
/// a closure can rely on both being `Some` and calling e.g.
/// [`create_toplevel`](ClientState::create_toplevel) directly. They are public
/// so a future leg can bind additional globals the same way.
pub struct ClientState {
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub wm_base: Option<xdg_wm_base::XdgWmBase>,
    /// Populated by the `Dispatch` impls; copied into [`ClientEvents`] and
    /// returned by [`spawn`].
    pub events: ClientEvents,
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
/// `socket` is the name [`wlr::Display::add_socket_auto`] returned. This
/// helper lowers it to a `UnixStream` on the **caller** thread — using the
/// same `XDG_RUNTIME_DIR` the server bound under — and moves the connected
/// stream into the spawned thread, which wraps it with
/// [`Connection::from_socket`]. No `WAYLAND_DISPLAY` (or any other
/// environment variable) is written: `wayland-client` 0.31's only
/// env-resolving constructor is [`Connection::connect_to_env`], and there is
/// no name-taking alternative, so `from_socket` is the explicit-path route.
/// That removes the `setenv`/`getenv` data race the earlier env-based version
/// carried; `connect_to_env` reads `WAYLAND_DISPLAY` *and* `XDG_RUNTIME_DIR`,
/// so writing the former from the child while the main thread raced into
/// libwayland was undefined behaviour.
///
/// The thread ends with two blocking round-trips. The first flushes everything
/// `drive` queued and waits for the server's sync reply, so `join` returns only
/// once the server has dispatched those requests; without it a fast server loop
/// could reach the caller's assertion while the last `commit` was still in the
/// client's outgoing buffer. The second is what makes the configure assertion
/// honest: wlroots schedules the answering configure from an *idle* source, so
/// it is queued a turn after the sync reply the first round-trip stops on and
/// is not in that read. The second round-trip (the server keeps running until
/// this thread finishes) reads and dispatches it, and the `xdg_surface`
/// `Dispatch` impl records and acks it before the thread returns.
///
/// Returns the observed [`ClientEvents`] via the `JoinHandle`.
pub fn spawn(
    socket: &str,
    drive: impl FnOnce(&mut ClientState, &QueueHandle<ClientState>) + Send + 'static,
) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to wayland socket");
    // Bound the blocking waits inside the thread. Without these a stuck hop
    // leaves `roundtrip` blocked forever, the thread never finishes, and CI
    // hangs where it should fail: the read call returns `TimedOut`/
    // `WouldBlock` after ten seconds, `roundtrip` surfaces the `Err`, and the
    // thread panics with the message below.
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("set read timeout on wayland socket");
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("set write timeout on wayland socket");
    std::thread::spawn(move || {
        let conn = Connection::from_socket(stream).expect("wrap wayland socket");
        let (globals, mut queue) =
            registry_queue_init::<ClientState>(&conn).expect("registry queue init");
        let qh = queue.handle();

        let mut state = ClientState {
            compositor: None,
            wm_base: None,
            events: ClientEvents::default(),
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

        // The first round-trip stops on the server's sync reply; the configure
        // wlroots scheduled from an idle source lands a turn later. A second
        // round-trip dispatches it (and anything else already queued) so the
        // assertions can see the configure/ack exchange, not just the
        // toplevel.
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server's configure is dispatched");

        state.events
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
        state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // xdg-shell requires every configure be acked; a later leg that waits
        // for a mapped window depends on it. Record both halves so the seed
        // test can assert the exchange happened, not merely that the role was
        // created.
        if let xdg_surface::Event::Configure { serial } = event {
            state.events.configure_events += 1;
            proxy.ack_configure(serial);
            state.events.acked_configures += 1;
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
