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

use std::os::fd::AsFd;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_subcompositor,
    wl_subsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
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
    /// The seat, when the server advertised one. Bound opportunistically by
    /// [`spawn`] so a driven client can send seat-parameterised requests such
    /// as `xdg_toplevel.show_window_menu`; `None` when no seat global exists.
    pub seat: Option<wl_seat::WlSeat>,
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

/// Drive a client that asks for a window menu after its toplevel is configured.
///
/// `xdg_toplevel.show_window_menu` is refused with "surface has not been
/// configured yet" until the initial configure has been acked, so this is the
/// two-phase shape [`spawn_mapped`] uses — create, bufferless commit,
/// round-trip so the configure is acked — with the menu request in phase two
/// instead of a buffer. Returns the observed [`ClientEvents`].
pub fn spawn_show_window_menu(
    socket: &str,
    x: i32,
    y: i32,
) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to wayland socket");
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
            seat: None,
            events: ClientEvents::default(),
        };
        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).expect("bind wl_seat");
        state.compositor = Some(compositor.clone());
        state.wm_base = Some(wm_base.clone());
        state.seat = Some(seat.clone());

        // Phase 1: role + bufferless commit, then a round-trip so the initial
        // configure is dispatched and acked.
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the initial configure is dispatched and acked");

        // Phase 2: the menu request, then a round-trip so the server has
        // observed it.
        let seat = state.seat.clone().expect("wl_seat");
        toplevel.show_window_menu(&seat, 0, x, y);
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the show-window-menu request");

        drop((surface, xdg_surface, toplevel, seat, wm_base, compositor));
        state.events
    })
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
            seat: None,
            events: ClientEvents::default(),
        };
        // `registry_queue_init` buffers the initial globals rather than
        // forwarding them to a handler, so the standard `GlobalList::bind` is
        // the way to bind them; binding here also guarantees both are present
        // before `drive` runs.
        state.compositor = Some(globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor"));
        state.wm_base = Some(globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base"));
        // Opportunistic: a seat exists only when the server called
        // `Runtime::create_seat`. A test that needs one creates it; the rest
        // leave this `None` and never ask for it.
        state.seat = globals.bind(&qh, 1..=1, ()).ok();

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

/// Like [`spawn`], but drives a toplevel all the way to **mapped**.
///
/// xdg-shell requires the role's first commit be bufferless, so the flow is
/// two-phase and needs a round-trip *between* the commits — which the
/// single-shot `drive` closure of [`spawn`] cannot do, since it is handed no
/// event queue. Phase one creates the role, commits bufferless, and
/// round-trips so the server's initial configure arrives and is acked (the
/// `Dispatch<xdg_surface>` impl acks it). Phase two creates an shm pool and
/// buffer, attaches and commits, then round-trips so the server has observed
/// the buffered commit and mapped the surface.
///
/// The proxy handles are held until the second round-trip has been answered,
/// so neither the buffer nor the surface is torn down before the server has
/// seen the map. Returns the observed [`ClientEvents`] via the `JoinHandle`.
#[allow(dead_code)]
pub fn spawn_mapped(socket: &str) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    // Built before the thread starts: `socket` is a borrow that cannot cross
    // into the `'static` thread, and the closure needs an owned path anyway.
    let shm_path = crate::common::isolated_runtime_dir().join(format!(
        "wlr-rs-shm-{}-{}",
        std::process::id(),
        socket
    ));
    let stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to wayland socket");
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
            seat: None,
            events: ClientEvents::default(),
        };
        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
        state.compositor = Some(compositor.clone());
        state.wm_base = Some(wm_base.clone());

        // Opportunistic: the global exists only when the server called
        // `Runtime::create_presentation`. Tests that never do are unchanged —
        // `bind` returns `Err(Missing)` and the client drives its surface
        // without asking for feedback. Version 1 is the client bindings'
        // maximum even though the server advertises 2.
        let presentation: Option<wp_presentation::WpPresentation> =
            globals.bind(&qh, 1..=1, ()).ok();

        // Phase 1: role + bufferless commit, then a round-trip so the server's
        // initial configure arrives and is acked.
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        // Ask for presentation feedback before the first commit: wlroots moves
        // a requested feedback from the surface's pending to its current state
        // during that commit's apply, so the server's commit handler observes
        // it. The proxy is held until after the second round-trip.
        let feedback = presentation
            .as_ref()
            .map(|presentation| presentation.feedback(&surface, &qh, ()));
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the initial configure is dispatched and acked");

        // Phase 2: an shm buffer, attached and committed. 64x64 ARGB8888 needs
        // a 16 KiB backing file; a regular file is a valid shm backing on Linux
        // (the server mmaps it from the fd the pool carries).
        const W: i32 = 64;
        const H: i32 = 64;
        const STRIDE: i32 = W * 4;
        let size = STRIDE * H;
        // Read-write, not `File::create`'s write-only: the server mmaps the fd
        // with `PROT_READ`, and mapping a write-only fd fails `EACCES`. libwayland
        // then rejects the pool with "Failed to create memory mapping".
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&shm_path)
            .expect("create shm backing file");
        file.set_len(size as u64).expect("size shm backing file");
        let pool = shm.create_pool(file.as_fd(), size, &qh, ());
        let buffer = pool.create_buffer(0, W, H, STRIDE, wl_shm::Format::Argb8888, &qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.damage(0, 0, W, H);
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the buffered commit and maps");

        // Keep every handle alive until the round-trip above has been answered,
        // then drop them here — after the map, not before it.
        drop((
            surface,
            xdg_surface,
            _toplevel,
            feedback,
            buffer,
            pool,
            shm,
            presentation,
            file,
        ));
        state.events
    })
}

/// Drive a client that creates a sub-surface.
///
/// Phase one creates an `xdg_toplevel` parent and commits it bufferless so the
/// server announces and tracks it — a tracked parent is what lets the server's
/// [`Surface::subsurface_parent_id`](wlr::Surface::subsurface_parent_id)
/// resolve. Phase two creates a child surface and a `wl_subsurface` on that
/// parent, commits the child bufferless, then round-trips so the server has
/// observed the child's `new_subsurface`. No buffer is attached: the role's
/// creation is the event under test, and mapping the child would add noise the
/// caller did not ask for.
///
/// The proxy handles are held until after the second round-trip, then dropped;
/// the connection closing on the thread's return destroys both surfaces.
/// Returns the observed [`ClientEvents`] via the `JoinHandle`.
pub fn spawn_subsurface(socket: &str) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to wayland socket");
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
            seat: None,
            events: ClientEvents::default(),
        };
        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        // `init_graphics` always creates the subcompositor, so the global is
        // present before any client connects.
        let subcompositor: wl_subcompositor::WlSubcompositor =
            globals.bind(&qh, 1..=1, ()).expect("bind wl_subcompositor");
        state.compositor = Some(compositor.clone());
        state.wm_base = Some(wm_base.clone());

        // Phase 1: an xdg-toplevel parent, committed bufferless and
        // round-tripped, so the server has announced and tracked its surface.
        let parent = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&parent, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server announces the parent toplevel");

        // Phase 2: the child surface and its sub-surface role. wlroots adds a
        // sub-surface to the parent's *current* state on the parent's next
        // commit — not when the object is created — and emits `new_subsurface`
        // from that apply, so the parent is committed again here. The child's
        // own bufferless commit is included because it is the real client
        // sequence, and the round-trip drains both. `set_position` is applied
        // from the sub-surface's pending state by that same parent commit, so
        // the server reads a non-default `(10, 20)` through the role.
        let child = compositor.create_surface(&qh, ());
        let subsurface = subcompositor.get_subsurface(&child, &parent, &qh, ());
        subsurface.set_position(10, 20);
        child.commit();
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the child subsurface");

        drop((
            child,
            subsurface,
            parent,
            xdg_surface,
            _toplevel,
            subcompositor,
        ));
        state.events
    })
}

/// Like [`spawn_subsurface`], but **maps** the child sub-surface with a small
/// shm buffer.
///
/// `wlr_surface_for_each_surface` only recurses into a sub-surface once it is
/// `mapped` (wlroots `types/wlr_compositor.c` skips an unmapped child), so the
/// bufferless child `spawn_subsurface` creates is never visited by the tree
/// walk. This helper exists for the traversal regression that needs a
/// non-root surface the walk actually yields.
pub fn spawn_subsurface_mapped(socket: &str) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let shm_path = crate::common::isolated_runtime_dir().join(format!(
        "wlr-rs-shm-{}-{}-child",
        std::process::id(),
        socket
    ));
    let stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to wayland socket");
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
            seat: None,
            events: ClientEvents::default(),
        };
        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let subcompositor: wl_subcompositor::WlSubcompositor =
            globals.bind(&qh, 1..=1, ()).expect("bind wl_subcompositor");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
        state.compositor = Some(compositor.clone());
        state.wm_base = Some(wm_base.clone());

        // Phase 1: a parent toplevel, committed bufferless so the server
        // announces and tracks it.
        let parent = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&parent, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server announces the parent toplevel");

        // Phase 2: map the **parent** with an shm buffer. A sub-surface only
        // maps once its parent is mapped (`subsurface_consider_map` checks
        // `parent->mapped`), so this must happen before the child.
        const W: i32 = 64;
        const H: i32 = 64;
        const STRIDE: i32 = W * 4;
        let size = STRIDE * H;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&shm_path)
            .expect("create shm backing file");
        file.set_len((2 * size) as u64)
            .expect("size shm backing file");
        let pool = shm.create_pool(file.as_fd(), 2 * size, &qh, ());
        let parent_buffer = pool.create_buffer(0, W, H, STRIDE, wl_shm::Format::Argb8888, &qh, ());
        let child_buffer =
            pool.create_buffer(size, W, H, STRIDE, wl_shm::Format::Argb8888, &qh, ());
        parent.attach(Some(&parent_buffer), 0, 0);
        parent.damage(0, 0, W, H);
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the parent maps");

        // Phase 3: the child sub-surface, mapped with its own buffer, then the
        // parent commit that folds it into the parent's current state.
        let child = compositor.create_surface(&qh, ());
        let subsurface = subcompositor.get_subsurface(&child, &parent, &qh, ());
        subsurface.set_position(10, 20);
        child.attach(Some(&child_buffer), 0, 0);
        child.damage(0, 0, W, H);
        child.commit();
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the mapped child subsurface");

        drop((
            child,
            subsurface,
            parent,
            xdg_surface,
            _toplevel,
            parent_buffer,
            child_buffer,
            pool,
            shm,
            subcompositor,
            file,
        ));
        state.events
    })
}

/// Like [`spawn_subsurface`], but destroys the **parent** surface while keeping
/// the child alive, then commits the child once more.
///
/// This is the destroy-order witness for the sub-surface role: wlroots frees the
/// `wlr_subsurface` from the parent surface's own `destroy` signal
/// (`subsurface_handle_parent_destroy`), while the child `wlr_surface` survives.
/// The extra child commit after the parent is gone gives the server a handler
/// that runs *after* the role object has been freed, so a trailing
/// `subsurface_parent_id`/`subsurface_parent_state` call there must miss rather
/// than dereference freed memory.
pub fn spawn_subsurface_then_destroy_parent(socket: &str) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to wayland socket");
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
            seat: None,
            events: ClientEvents::default(),
        };
        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let subcompositor: wl_subcompositor::WlSubcompositor =
            globals.bind(&qh, 1..=1, ()).expect("bind wl_subcompositor");
        state.compositor = Some(compositor.clone());
        state.wm_base = Some(wm_base.clone());

        // Parent toplevel, committed bufferless so the server tracks it.
        let parent = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&parent, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server announces the parent toplevel");

        // Child sub-surface, committed so the server observes `new_subsurface`.
        let child = compositor.create_surface(&qh, ());
        let subsurface = subcompositor.get_subsurface(&child, &parent, &qh, ());
        child.commit();
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the child subsurface");

        // xdg-shell requires a surface's role objects be destroyed before the
        // surface itself, so tear the toplevel and its xdg_surface down first,
        // then destroy the now-roleless parent surface while keeping the child.
        // The extra child commit gives the server a handler that runs after
        // wlroots has freed the role object.
        _toplevel.destroy();
        xdg_surface.destroy();
        parent.destroy();
        child.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the post-destroy child commit");

        drop((child, subsurface, subcompositor));
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

impl Dispatch<wl_seat::WlSeat, ()> for ClientState {
    /// Carries `capabilities`/`name`; the harness sends `show_window_menu`
    /// without waiting on them, so nothing is recorded.
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_subcompositor::WlSubcompositor, ()> for ClientState {
    /// Carries no events; bound only so `get_subsurface` can be called.
    fn event(
        _state: &mut Self,
        _proxy: &wl_subcompositor::WlSubcompositor,
        _event: wl_subcompositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_subsurface::WlSubsurface, ()> for ClientState {
    /// Carries no events in this flow; the role is created and committed only.
    fn event(
        _state: &mut Self,
        _proxy: &wl_subsurface::WlSubsurface,
        _event: wl_subsurface::Event,
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

impl Dispatch<wl_shm::WlShm, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for ClientState {
    /// Fires the `clock_id` event once on bind; nothing to record.
    fn event(
        _state: &mut Self,
        _proxy: &wp_presentation::WpPresentation,
        _event: wp_presentation::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for ClientState {
    /// `presented`/`discarded`/`sync_output`. The server may destroy the
    /// feedback (sending `discarded`) once the commit handler samples it; the
    /// events are drained by the round-trips and need no action here.
    fn event(
        _state: &mut Self,
        _proxy: &wp_presentation_feedback::WpPresentationFeedback,
        _event: wp_presentation_feedback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
