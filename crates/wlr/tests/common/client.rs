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
use std::os::unix::net::UnixStream;

use wayland_client::globals::{BindError, GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_subcompositor, wl_subsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1,
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1,
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1, ext_workspace_handle_v1, ext_workspace_manager_v1,
};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::wp::security_context::v1::client::{
    wp_security_context_manager_v1, wp_security_context_v1,
};
use wayland_protocols::wp::tearing_control::v1::client::{
    wp_tearing_control_manager_v1, wp_tearing_control_v1,
};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols::xdg::toplevel_icon::v1::client::{
    xdg_toplevel_icon_manager_v1, xdg_toplevel_icon_v1,
};
use wayland_protocols::xdg::toplevel_tag::v1::client::xdg_toplevel_tag_manager_v1;
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

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
    /// Whether the activation client received the `done` event carrying its
    /// token string — the server-side half of the xdg-activation round trip.
    pub activation_token_received: bool,
    /// Whether the activation client then redeemed that token with `activate`.
    pub activation_sent: bool,
    /// Whether a layer-shell client observed the server's `closed` event after
    /// the compositor destroyed its layer surface.
    pub layer_closed: bool,
    /// Whether a presentation-feedback client observed the server's
    /// `discarded` event for its feedback. The server sends it when the
    /// feedback is destroyed without being presented — which is what the
    /// server-side `sampled()` + drop path does — so a mapped client that
    /// asked for feedback observes it on a later round-trip.
    pub feedback_discarded: bool,
    /// Whether a presentation-feedback client observed the server's
    /// `presented` event for its feedback. The server sends it when the
    /// commit handler reports the sample via `send_presented` — the
    /// counterpart to [`feedback_discarded`](Self::feedback_discarded),
    /// which the drop-without-send path produces instead.
    pub feedback_presented: bool,
}

/// What a `zwlr_foreign_toplevel_manager_v1` client observed, returned by
/// [`spawn_foreign_toplevel`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForeignToplevelEvents {
    /// How many `toplevel` events the manager delivered.
    pub toplevels_seen: u32,
    /// The last `title` event's string.
    pub title: Option<String>,
    /// The last `app_id` event's string.
    pub app_id: Option<String>,
    /// Whether at least one `state` array arrived.
    pub state_events: u32,
    /// Whether the initial `done` arrived.
    pub saw_done: bool,
    /// Whether the server's `closed` event arrived. The driven flow ends with
    /// the client's own `close()` request, which the server answers with
    /// `closed`; tests assert it arrived, proving the recording arm fires.
    /// No request is driven after it, since `close()` was the last one sent.
    pub saw_closed: bool,
    /// How many `parent` events arrived. The bind-time replay sends exactly
    /// one (naming no parent — the handle is never parented); recorded rather
    /// than swallowed so a replay that parents one shows up instead of
    /// vanishing into `_`.
    pub parent_events: u32,
}

/// What an `ext_foreign_toplevel_list_v1` client observed, returned by
/// [`spawn_ext_foreign_toplevel`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtForeignToplevelEvents {
    /// How many `toplevel` events the list delivered.
    pub toplevels_seen: u32,
    /// The last `title` event's string.
    pub title: Option<String>,
    /// The last `app_id` event's string.
    pub app_id: Option<String>,
    /// The last `identifier` event's string — wlroots generates a stable token.
    pub identifier: Option<String>,
    /// Whether the initial `done` arrived.
    pub saw_done: bool,
    /// Whether the server's `closed` event arrived. The observed handle is
    /// never closed during the run; recorded rather than swallowed so a close
    /// shows up instead of vanishing into `_`.
    pub saw_closed: bool,
}

/// What an `ext_session_lock_v1` client observed, returned by
/// [`spawn_session_lock`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionLockEvents {
    /// The last `configure` event's `(serial, width, height)`.
    pub configure: Option<(u32, u32, u32)>,
    /// The serial the driver acked after the first round-trip (the configure
    /// above may be a later one, re-sent after the map).
    pub acked_serial: Option<u32>,
    /// Whether the lock's `locked` event arrived — the server sends it once
    /// every output is covered by a mapped lock surface.
    pub saw_locked: bool,
    /// Whether the lock's `finished` event arrived. The locker never unlocks
    /// during the run; recorded rather than swallowed.
    pub saw_finished: bool,
}

/// What an `ext_workspace_manager_v1` client observed, returned by
/// [`spawn_ext_workspace`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtWorkspaceEvents {
    /// How many `workspace_group` events the manager delivered.
    pub groups_seen: u32,
    /// How many `workspace` events the manager delivered.
    pub workspaces_seen: u32,
    /// Whether the initial `done` arrived.
    pub saw_done: bool,
    /// Whether the manager's `finished` event arrived. The manager outlives
    /// the run, so tests assert this stayed false.
    pub saw_finished: bool,
    /// Whether the replayed group's `removed` event arrived. The group is
    /// never removed during the run; recorded rather than swallowed.
    pub group_removed: bool,
    /// Whether the replayed workspace's `removed` event arrived. The
    /// workspace is never removed during the run; recorded rather than
    /// swallowed.
    pub workspace_removed: bool,
}

/// The globals a driven client has bound.
///
/// The two `Option`s are filled by [`spawn`] before the `drive` closure runs;
/// a closure can rely on both being `Some` and calling e.g.
/// [`create_toplevel`](ClientState::create_toplevel) directly. They are public
/// so a future leg can bind additional globals the same way.
#[derive(Default)]
pub struct ClientState {
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub wm_base: Option<xdg_wm_base::XdgWmBase>,
    /// The seat, when the server advertised one. Bound opportunistically by
    /// [`spawn`] so a driven client can send seat-parameterised requests such
    /// as `xdg_toplevel.show_window_menu`; `None` when no seat global exists.
    pub seat: Option<wl_seat::WlSeat>,
    /// The token string the activation round trip received from the server's
    /// `xdg_activation_token_v1.done` event, held between the round trip that
    /// answers `commit` and the one that sends `activate`.
    pub activation_token: Option<String>,
    /// Populated by the `Dispatch` impls; copied into [`ClientEvents`] and
    /// returned by [`spawn`].
    pub events: ClientEvents,
    /// What a foreign-toplevel-management client observed; returned by
    /// [`spawn_foreign_toplevel`].
    pub foreign: ForeignToplevelEvents,
    /// The exported foreign-toplevel handle the server replayed on bind, kept
    /// alive so the driven client can drive its requests.
    pub foreign_handle: Option<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1>,
    /// What an `ext-foreign-toplevel-list` client observed.
    pub ext_foreign: ExtForeignToplevelEvents,
    /// The ext-foreign-toplevel handle the list replayed on bind.
    pub ext_foreign_handle: Option<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1>,
    /// What an `ext-workspace` client observed.
    pub ext_workspace: ExtWorkspaceEvents,
    /// The workspace group the manager replayed on bind.
    pub ext_workspace_group: Option<ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1>,
    /// The workspace the manager replayed on bind.
    pub ext_workspace_handle: Option<ext_workspace_handle_v1::ExtWorkspaceHandleV1>,
    /// What an `ext-session-lock` client observed; returned by
    /// [`spawn_session_lock`].
    pub session_lock: SessionLockEvents,
}

impl ClientState {
    /// The in-thread preamble every `spawn_*` driver shares.
    ///
    /// Wraps the caller-connected `stream` (connected on the caller thread via
    /// [`crate::common::connect_socket` — the socket name borrows from the
    /// test and `XDG_RUNTIME_DIR` is resolved there), runs the registry
    /// round-trip, and hands back a default state. Returns the connection too:
    /// it must stay alive until the thread returns, so the caller binds it to
    /// a named `_conn` that outlives the driven requests. Per-protocol binds
    /// stay inline in each driver, against the returned [`GlobalList`].
    fn fresh(stream: UnixStream) -> (Connection, GlobalList, EventQueue<Self>, Self) {
        let conn = Connection::from_socket(stream).expect("wrap wayland socket");
        let (globals, queue) = registry_queue_init::<Self>(&conn).expect("registry queue init");
        (conn, globals, queue, Self::default())
    }

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

    /// Like [`create_toplevel`](Self::create_toplevel), but the client asks
    /// for every requestable state — maximized, fullscreen and minimized —
    /// before the first commit, so the server's `requested` snapshot reads
    /// all trues. `set_fullscreen(None)` names no output, which the protocol
    /// allows and wlroots records as a bare fullscreen request.
    pub fn create_toplevel_with_requests(&mut self, qh: &QueueHandle<Self>) {
        let compositor = self.compositor.as_ref().expect("wl_compositor not bound");
        let wm_base = self.wm_base.as_ref().expect("xdg_wm_base not bound");
        let surface = compositor.create_surface(qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());
        toplevel.set_maximized();
        toplevel.set_fullscreen(None);
        toplevel.set_minimized();
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
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

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

/// Drive a `zwlr_foreign_toplevel_manager_v1` client against a handle the
/// compositor exported before the connection.
///
/// Binds the manager and round-trips so the server's bind-time replay of the
/// exported handle — its `toplevel` event plus title, app id, state and `done`
/// — is dispatched, then drives every handle request the protocol offers except
/// `destroy`. The requests are round-tripped so the server observes them before
/// the thread returns. Returns the observed [`ForeignToplevelEvents`].
pub fn spawn_foreign_toplevel(socket: &str) -> std::thread::JoinHandle<ForeignToplevelEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).expect("bind wl_seat");
        let manager: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1 = globals
            .bind(&qh, 1..=3, ())
            .expect("bind zwlr_foreign_toplevel_manager_v1");
        state.compositor = Some(compositor.clone());
        state.seat = Some(seat.clone());

        // The manager replays every toplevel that already exists when a client
        // binds, then its details, then `done`.
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the replayed handle is dispatched");

        let surface = compositor.create_surface(&qh, ());
        let handle = state
            .foreign_handle
            .as_ref()
            .expect("the manager replayed a handle")
            .clone();
        handle.set_maximized();
        handle.set_minimized();
        handle.unset_minimized();
        handle.set_fullscreen(None);
        handle.activate(&seat);
        handle.set_rectangle(&surface, 5, 6, 7, 8);
        handle.close();
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the requests");

        drop((surface, handle, manager, seat, compositor));
        state.foreign
    })
}

/// Drive a `zwlr_foreign_toplevel_manager_v1` client whose `set_rectangle`
/// names a surface the compositor tracks.
///
/// Binds the manager and round-trips the bind-time replay, then builds an
/// xdg-toplevel parent and a bufferless child subsurface — both committed and
/// round-tripped, so the server has announced and tracked each surface — and
/// sends `set_rectangle` with the **child** surface and a distinctive
/// rectangle. The server resolves it to `Some` id, unlike the untracked
/// surface [`spawn_foreign_toplevel`] rectangles. A final `close()` drops the
/// server-side handle, matching that driver's end state. Returns the observed
/// [`ForeignToplevelEvents`].
pub fn spawn_foreign_toplevel_tracked_rectangle(
    socket: &str,
) -> std::thread::JoinHandle<ForeignToplevelEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let subcompositor: wl_subcompositor::WlSubcompositor =
            globals.bind(&qh, 1..=1, ()).expect("bind wl_subcompositor");
        let manager: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1 = globals
            .bind(&qh, 1..=3, ())
            .expect("bind zwlr_foreign_toplevel_manager_v1");

        // The manager replays every toplevel that already exists when a client
        // binds, then its details, then `done`.
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the replayed handle is dispatched");

        // An xdg-toplevel parent, committed bufferless and round-tripped, so
        // the server has announced and tracked its surface.
        let parent = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&parent, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server announces the parent toplevel");

        // The child subsurface, whose role the server announces (and tracks)
        // on the parent's next commit.
        let child = compositor.create_surface(&qh, ());
        let subsurface = subcompositor.get_subsurface(&child, &parent, &qh, ());
        child.commit();
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the child subsurface");

        let handle = state
            .foreign_handle
            .as_ref()
            .expect("the manager replayed a handle")
            .clone();
        handle.set_rectangle(&child, 11, 22, 33, 44);
        handle.close();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the rectangle and close");

        drop((
            child,
            subsurface,
            parent,
            xdg_surface,
            handle,
            manager,
            compositor,
        ));
        state.foreign
    })
}

/// Drive an `ext_foreign_toplevel_list_v1` client against a handle the
/// compositor exported before the connection.
///
/// Binds the list and round-trips so the server's bind-time replay — the
/// `toplevel` event and the handle's title, app id, identifier and `done` — is
/// dispatched. The list has no client requests (the protocol is observation
/// only), so nothing is driven. Returns the observed
/// [`ExtForeignToplevelEvents`].
pub fn spawn_ext_foreign_toplevel(
    socket: &str,
) -> std::thread::JoinHandle<ExtForeignToplevelEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let list: ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind ext_foreign_toplevel_list_v1");

        // The list replays every exported toplevel on bind, then its details,
        // then `done`.
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the replayed handle is dispatched");

        drop((list,));
        state.ext_foreign
    })
}

/// Drive an `ext_workspace_manager_v1` client that observes the workspace
/// objects the compositor created and then commits a batch of requests.
///
/// Binds the manager and round-trips so the bind-time replay of the group and
/// workspace is dispatched, then sends every workspace request the protocol
/// offers and one `commit`. Each request is round-tripped to the server by the
/// final `commit`'s round-trip. Returns the observed [`ExtWorkspaceEvents`].
pub fn spawn_ext_workspace(socket: &str) -> std::thread::JoinHandle<ExtWorkspaceEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let manager: ext_workspace_manager_v1::ExtWorkspaceManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind ext_workspace_manager_v1");

        // The manager replays every group and workspace that already exists on
        // bind, then `done`.
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the replayed objects are dispatched");

        let group = state
            .ext_workspace_group
            .as_ref()
            .expect("the manager replayed a group")
            .clone();
        let workspace = state
            .ext_workspace_handle
            .as_ref()
            .expect("the manager replayed a workspace")
            .clone();

        // One atomic batch: the compositor sees all five requests when the
        // commit drains them.
        workspace.activate();
        workspace.deactivate();
        workspace.assign(&group);
        group.create_workspace("wlr-new".to_owned());
        workspace.remove();
        manager.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the committed requests");

        drop((workspace, group, manager));
        state.ext_workspace
    })
}

/// Drive an `ext_workspace_manager_v1` client that binds the manager and
/// commits nothing.
///
/// Binds the manager and round-trips so the bind-time replay of the group and
/// workspace is dispatched, then sends one bare `commit` with no staged
/// requests. The server still emits its commit signal for the empty batch, so
/// this is the baseline that distinguishes "no requests" from "all requests
/// stale". Returns the observed [`ExtWorkspaceEvents`].
pub fn spawn_empty_commit(socket: &str) -> std::thread::JoinHandle<ExtWorkspaceEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let manager: ext_workspace_manager_v1::ExtWorkspaceManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind ext_workspace_manager_v1");

        // The manager replays every group and workspace that already exists on
        // bind, then `done`.
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the replayed objects are dispatched");

        // Commit with nothing staged: the server drains an empty batch.
        manager.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the empty commit");

        drop((manager,));
        state.ext_workspace
    })
}

/// Drive a `wp_security_context_manager_v1` client that creates a security
/// context with a real listening socket, attaches its metadata and commits it.
///
/// The commit is the only thing the server observes: `wp_security_context_v1`
/// carries no events, so the thread returns nothing. A round-trip after the
/// commit makes `join` wait until the server has dispatched it. The listening
/// socket and the pipe end kept alive until after the round-trip are what make
/// the context valid — the compositor accepts on the listen fd and watches the
/// close fd for hangup.
pub fn spawn_security_context(socket: &str) -> std::thread::JoinHandle<()> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let manager: wp_security_context_manager_v1::WpSecurityContextManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind wp_security_context_manager_v1");

        // A listening socket for the sandbox connection the compositor would
        // accept, and a pipe whose read end is the hangup signal. The path is
        // unique per thread; any stale file is unlinked before binding (a
        // crashed run leaves the socket file behind) and removed again when
        // the thread exits. Contained by the `0700` runtime directory.
        let listen_path = crate::common::shm_path_for("security-context-listen");
        let _ = std::fs::remove_file(&listen_path);
        let listener = std::os::unix::net::UnixListener::bind(&listen_path)
            .expect("bind the security-context listen socket");
        let (close_read, close_write) = rustix::pipe::pipe().expect("security-context close pipe");

        let context = manager.create_listener(listener.as_fd(), close_read.as_fd(), &qh, ());
        context.set_sandbox_engine("org.wlr.test".to_owned());
        context.set_app_id("org.wlr.test.app".to_owned());
        context.set_instance_id("instance-1".to_owned());
        context.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the committed context");

        drop((context, listener, close_write, manager));
        // Best-effort: unlink the listen socket now that it is closed.
        let _ = std::fs::remove_file(&listen_path);
    })
}

/// Drive a real `ext_session_lock_v1` locker: lock, create a lock surface on
/// the advertised output, wait for its configure, ack it, and commit a buffer
/// so the server applies the ack and maps the surface.
///
/// The server configures the lock surface to the output's current size when
/// the role is created, so the returned [`SessionLockEvents::configure`]
/// is the exact `(serial, width, height)` the server-side `LockSurface`
/// accessors must agree with. Two round-trips separate the phases the way
/// [`spawn`] documents: the first flushes the lock requests and dispatches
/// the configure; the second dispatches the ack and the mapped commit.
/// Proxies are held until the second round-trip is answered.
pub fn spawn_session_lock(socket: &str) -> std::thread::JoinHandle<SessionLockEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let shm_path = crate::common::shm_path_for(&format!("{socket}-lock"));
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let manager: ext_session_lock_manager_v1::ExtSessionLockManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind ext_session_lock_manager_v1");
        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
        // The headless output is announced when the server's backend starts —
        // after this thread already connected and listed globals — so it is
        // not in the initial list. Re-list until it appears: every round-trip
        // dispatches the registry events the server sent since the last one,
        // and the server keeps running while this thread is alive, so each
        // iteration makes progress rather than blocking forever.
        let output: wl_output::WlOutput = {
            let mut bound = None;
            for _ in 0..30 {
                match globals.bind(&qh, 1..=4, ()) {
                    Ok(output) => {
                        bound = Some(output);
                        break;
                    }
                    Err(BindError::NotPresent) => {
                        queue
                            .roundtrip(&mut state)
                            .expect("roundtrip for late globals");
                    }
                    Err(e) => panic!("bind wl_output: {e}"),
                }
            }
            bound.expect("bind wl_output")
        };

        let lock = manager.lock(&qh, ());
        let surface = compositor.create_surface(&qh, ());
        let lock_surface = lock.get_lock_surface(&surface, &output, &qh, ());

        // The role (and the server's first configure) exists once the server
        // has seen `get_lock_surface` — committing before that configure
        // arrives is a protocol error ("has never been configured"). So the
        // first round-trip carries no commit: it lets the server create the
        // role and dispatches its configure.
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server creates the lock surface and configures");
        let serial = state
            .session_lock
            .configure
            .map(|(serial, width, height)| (serial, width as i32, height as i32))
            .expect("the server configured the lock surface");
        lock_surface.ack_configure(serial.0);
        state.session_lock.acked_serial = Some(serial.0);

        // Unlike xdg-shell (bufferless first commit), the committed lock
        // surface must carry a buffer — wlroots raises a protocol error on a
        // null-buffer commit — and its dimensions must match the acked
        // configure, so the shm buffer is sized from the configure the server
        // just sent rather than a constant. This commit is also what applies
        // the ack to the server's `current` and maps the surface.
        let (_, w, h) = serial;
        let stride = w * 4;
        let size = stride * h;
        let file = crate::common::create_shm_backing(&shm_path, size as u64);
        let pool = shm.create_pool(file.as_fd(), size, &qh, ());
        let buffer = pool.create_buffer(0, w, h, stride, wl_shm::Format::Argb8888, &qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.damage(0, 0, w, h);
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server applies the ack and maps");

        // Held until the round-trip above was answered, then dropped here —
        // after the map, not before it. The session stays locked after the
        // locker goes away (the crash-stays-locked rule); the test tears the
        // whole compositor down afterwards.
        drop((
            lock_surface,
            surface,
            lock,
            output,
            buffer,
            pool,
            shm,
            manager,
            file,
        ));
        let _ = std::fs::remove_file(&shm_path);
        state.session_lock
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
    // `connect_socket` bounds the blocking waits inside the thread (see
    // `IO_TIMEOUT`): without them a stuck hop leaves `roundtrip` blocked
    // forever, the thread never finishes, and CI hangs where it should fail.
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        // `registry_queue_init` buffers the initial globals rather than
        // forwarding them to a handler, so the standard `GlobalList::bind` is
        // the way to bind them; binding here also guarantees both are present
        // before `drive` runs.
        state.compositor = Some(globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor"));
        state.wm_base = Some(globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base"));
        // Opportunistic: a seat exists only when the server called
        // `Runtime::create_seat`. A test that needs one creates it; the rest
        // leave this `None` and never ask for it. Only the not-advertised
        // case may fall through to `None` — a version mismatch must fail at
        // the bind site rather than as a confusing `None` downstream.
        state.seat = match globals.bind(&qh, 1..=1, ()) {
            Ok(seat) => Some(seat),
            Err(BindError::NotPresent) => None,
            Err(e) => panic!("bind wl_seat: {e}"),
        };

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
// Shared harness: not every test binary maps a toplevel, so binaries that never call this would warn without the allow.
#[allow(dead_code)]
pub fn spawn_mapped(socket: &str) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    // Built before the thread starts: `socket` is a borrow that cannot cross
    // into the `'static` thread, and the closure needs an owned path anyway.
    let shm_path = crate::common::shm_path_for(&format!("{socket}-mapped"));
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
        state.compositor = Some(compositor.clone());
        state.wm_base = Some(wm_base.clone());

        // Opportunistic: the global exists only when the server called
        // `Runtime::create_presentation`. Tests that never do are unchanged —
        // only the not-advertised case falls through to `None`; a version
        // mismatch panics at the bind site. Version 1 is the client
        // bindings' maximum even though the server advertises 2.
        let presentation: Option<wp_presentation::WpPresentation> =
            match globals.bind(&qh, 1..=1, ()) {
                Ok(presentation) => Some(presentation),
                Err(BindError::NotPresent) => None,
                Err(e) => panic!("bind wp_presentation: {e}"),
            };

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
        let file = crate::common::create_shm_backing(&shm_path, size as u64);
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
        // Best-effort: the backing file is unlinked now that every handle is
        // closed, so a crashed run cannot leave a stale file behind.
        let _ = std::fs::remove_file(&shm_path);
        state.events
    })
}

/// Which tearing-control setup a mapped client performs before its first
/// commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TearingSetup {
    /// No tearing-control object: byte-identical to [`spawn_mapped`].
    #[default]
    None,
    /// Create a control object for the toplevel surface before the first
    /// commit; the hint stays at the vsync default.
    Control,
    /// Create a control object and set the async hint before the first
    /// commit, so the server observes the async half of the hint contract.
    Async,
}

/// Like [`spawn_mapped`], but the client also performs `setup`'s
/// tearing-control requests before its first commit.
///
/// The two-phase mapped flow is otherwise unchanged — role plus feedback
/// request and bufferless commit first, shm-backed map second — so every
/// other server observation (feedback, configure, map) behaves exactly as
/// under `spawn_mapped`. The control and manager proxies are held until the
/// second round-trip has been answered, like every other handle here.
pub fn spawn_mapped_with_tearing(
    socket: &str,
    setup: TearingSetup,
) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    // Built before the thread starts: `socket` is a borrow that cannot cross
    // into the `'static` thread, and the closure needs an owned path anyway.
    let shm_path = crate::common::shm_path_for(&format!("{socket}-mapped-tearing"));
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
        state.compositor = Some(compositor.clone());
        state.wm_base = Some(wm_base.clone());

        // Opportunistic, as for `wp_presentation` in `spawn_mapped`: only the
        // not-advertised case falls through to `None`.
        let presentation: Option<wp_presentation::WpPresentation> =
            match globals.bind(&qh, 1..=1, ()) {
                Ok(presentation) => Some(presentation),
                Err(BindError::NotPresent) => None,
                Err(e) => panic!("bind wp_presentation: {e}"),
            };
        // Required whenever the setup creates a control: the test created
        // the manager global, so a missing bind is a harness bug, not an
        // optional path.
        let tearing_manager: Option<wp_tearing_control_manager_v1::WpTearingControlManagerV1> =
            match setup {
                TearingSetup::None => None,
                TearingSetup::Control | TearingSetup::Async => Some(
                    globals
                        .bind(&qh, 1..=1, ())
                        .expect("bind wp_tearing_control_manager_v1"),
                ),
            };

        // Phase 1: role + bufferless commit, then a round-trip so the server's
        // initial configure arrives and is acked.
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        // Ask for presentation feedback before the first commit, as in
        // `spawn_mapped`.
        let feedback = presentation
            .as_ref()
            .map(|presentation| presentation.feedback(&surface, &qh, ()));
        // The control object exists from this request on; creating it before
        // the first commit means the server's commit handler observes it.
        let control = tearing_manager
            .as_ref()
            .map(|manager| manager.get_tearing_control(&surface, &qh, ()));
        if setup == TearingSetup::Async {
            control
                .as_ref()
                .expect("the async setup creates a control object")
                .set_presentation_hint(wp_tearing_control_v1::PresentationHint::Async);
        }
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the initial configure is dispatched and acked");

        // Phase 2: an shm buffer, attached and committed — same shape as
        // `spawn_mapped`, so the map the server observes is unchanged.
        const W: i32 = 64;
        const H: i32 = 64;
        const STRIDE: i32 = W * 4;
        let size = STRIDE * H;
        let file = crate::common::create_shm_backing(&shm_path, size as u64);
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
            control,
            tearing_manager,
            buffer,
            pool,
            shm,
            presentation,
            file,
        ));
        // Best-effort: the backing file is unlinked now that every handle is
        // closed, so a crashed run cannot leave a stale file behind.
        let _ = std::fs::remove_file(&shm_path);
        state.events
    })
}

/// Drive a `zwlr_layer_shell_v1` client through a layer surface's whole
/// client-observable life.
///
/// Phase one creates the layer surface, states its anchors/zone/size/keyboard
/// mode, and commits bufferless; a round trip lets the server's initial
/// configure arrive and be acked. Phase two round-trips twice more, giving the
/// server turns in which to destroy the surface through
/// `Runtime::destroy_layer_surface`; the client records the `closed` event and
/// then disconnects. Returns the observed [`ClientEvents`] via the
/// `JoinHandle`.
pub fn spawn_layer_surface(socket: &str) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals
            .bind(&qh, 1..=4, ())
            .expect("bind zwlr_layer_shell_v1");
        state.compositor = Some(compositor.clone());

        let surface = compositor.create_surface(&qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "wlr-rs-layer-test".to_owned(),
            &qh,
            (),
        );
        layer_surface.set_size(64, 48);
        layer_surface.set_anchor(zwlr_layer_surface_v1::Anchor::Top);
        layer_surface.set_exclusive_zone(32);
        layer_surface
            .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
        surface.commit();

        // Round trip one: the server announces and configures the surface, and
        // the `Dispatch` impl below acks that configure.
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server announces and configures the layer surface");
        // Two more round trips: each wakes the server for a turn in which its
        // `LoopHandler::should_stop` destroys the layer surface. The `closed`
        // event arrives on one of these.
        for _ in 0..3 {
            queue
                .roundtrip(&mut state)
                .expect("roundtrip so the server can destroy the layer surface");
        }

        drop((
            layer_surface,
            surface,
            layer_shell,
            compositor,
            state.compositor.take(),
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
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

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
///
/// Phase 4 commits the child once more *after* the role was announced, with
/// its buffer re-attached so it stays mapped: the server's `surface_committed`
/// handler then runs against a live role, which the commit-time accessor test
/// in `tests/subsurfaces.rs` asserts on.
pub fn spawn_subsurface_mapped(socket: &str) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let shm_path = crate::common::shm_path_for(&format!("{socket}-child"));
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

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
        let file = crate::common::create_shm_backing(&shm_path, (2 * size) as u64);
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

        // Phase 4: one more child commit after the role was announced. The
        // child was installed when phase 3's parent commit emitted
        // `new_subsurface`, so this commit reaches the server's
        // `surface_committed` with the role live — unlike phase 3's, which
        // ran before the install. The buffer is re-attached so the child
        // stays mapped; the parent commit re-applies the `(10, 20)` position.
        child.attach(Some(&child_buffer), 0, 0);
        child.damage(0, 0, W, H);
        child.commit();
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the post-announce child commit");

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
        // Best-effort: the backing file is unlinked now that every handle is
        // closed, so a crashed run cannot leave a stale file behind.
        let _ = std::fs::remove_file(&shm_path);
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
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

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

/// Like [`spawn_subsurface`], but the parent is a roleless plain surface: no
/// xdg role, only a bufferless commit.
///
/// The server tracks surfaces exclusively through role announce sites (an
/// xdg toplevel, layer surface, popup, …), so a roleless parent is never
/// installed: it gets no surface-id addon, no `new_subsurface` listener, and
/// the child is never announced or committed server-side. The test in
/// `tests/subsurfaces.rs` asserts that invisibility — no toplevel, no
/// `new_subsurface`, no commit, no configure — which is the observable half
/// of the untracked-parent case. (The other half, reading
/// `subsurface_parent_id() == None` alongside a `Some` position through a
/// handler, is unexpressible: any child the server can hand a handler
/// necessarily descends from an installed parent, and installation always
/// attaches the addon — see that test.)
pub fn spawn_subsurface_on_roleless_parent(socket: &str) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        // `init_graphics` always creates the subcompositor, so the global is
        // present before any client connects. No `xdg_wm_base` bind: nothing
        // here takes an xdg role.
        let subcompositor: wl_subcompositor::WlSubcompositor =
            globals.bind(&qh, 1..=1, ()).expect("bind wl_subcompositor");
        state.compositor = Some(compositor.clone());

        // A plain parent: created and committed with no role, so the server
        // never announces or tracks it.
        let parent = compositor.create_surface(&qh, ());
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server ignores the roleless parent");

        // The child sub-surface at `(10, 20)`, committed like the real client
        // sequence, then the parent commit that would fold it in.
        let child = compositor.create_surface(&qh, ());
        let subsurface = subcompositor.get_subsurface(&child, &parent, &qh, ());
        subsurface.set_position(10, 20);
        child.commit();
        parent.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server ignores the untracked child");

        drop((child, subsurface, parent, subcompositor));
        state.events
    })
}

/// Drive the xdg-activation round trip: mint a token, commit it, read the
/// server-generated name out of the `done` event, then redeem it with
/// `activate`.
///
/// xdg-activation lets a client hand another client permission to request
/// focus. The token is minted here, committed bufferlessly (no seat or surface
/// attached, so wlroots generates the name without an input-serial check),
/// named back by the server's `done`, and immediately redeemed by the same
/// connection with `activate`. The server's `request_activate` handler firing
/// is the proof the round trip completed; [`ClientEvents`] records both client
/// halves.
pub fn spawn_activation_round_trip(socket: &str) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let activation: xdg_activation_v1::XdgActivationV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind xdg_activation_v1");
        state.compositor = Some(compositor.clone());

        // A plain surface to activate. It need not be a mapped toplevel: the
        // protocol lets a client name any surface, and the server resolves it
        // to `None` when it is not a tracked toplevel.
        let surface = compositor.create_surface(&qh, ());
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server tracks the surface");

        // Mint, then commit, then round-trip so the `done` event carrying the
        // token string is dispatched before it is redeemed.
        let token = activation.get_activation_token(&qh, ());
        token.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sends the token `done`");

        let name = state
            .activation_token
            .take()
            .expect("the server sent a token name");
        activation.activate(name, &surface);
        state.events.activation_sent = true;
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the activate request");

        drop((surface, token, activation, compositor));
        state.events
    })
}

/// Drive the xdg-toplevel-icon and xdg-toplevel-tag state machines together.
///
/// Creates a mapped toplevel, attaches an icon carrying both a stock name and
/// a 64x64 shm pixel buffer, sets it with `xdg_toplevel_icon_manager_v1`,
/// then resets it with `set_icon(None)` and a second commit, then sets a tag
/// and description with `xdg_toplevel_tag_manager_v1` followed by an empty tag
/// and an empty description. The icon is double-buffered, so both the set and
/// the reset are applied by the surface commit that follows each; the tag and
/// description (including the empty ones) are applied when their requests
/// arrive. Each phase is round-tripped so the server has observed it before
/// the client returns. No [`ClientEvents`] field records the exchange — the
/// server-side handler assertions are the proof.
///
/// This is [`spawn_toplevel_meta_with`] with the tag phases on; the icon-only
/// form exists so a test can cover the icon clear path without creating a tag
/// manager at all.
pub fn spawn_toplevel_meta(socket: &str) -> std::thread::JoinHandle<ClientEvents> {
    spawn_toplevel_meta_with(socket, true)
}

/// Like [`spawn_toplevel_meta`], but the tag phases — and the tag-manager
/// bind they need — run only when `with_tag` is set. With `false` the driver
/// is the shared icon set/reset flow on its own: the clearing leg reuses it
/// instead of forking a second driver for the same phases.
pub fn spawn_toplevel_meta_with(
    socket: &str,
    with_tag: bool,
) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let shm_path = crate::common::shm_path_for(&format!("{socket}-meta"));
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
        let icon_manager: xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind xdg_toplevel_icon_manager_v1");
        // Bound only when the tag phases will run: binding a global the
        // server never advertised fails, and the icon-only leg creates no tag
        // manager.
        let tag_manager: Option<xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1> = with_tag
            .then(|| {
                globals
                    .bind(&qh, 1..=1, ())
                    .expect("bind xdg_toplevel_tag_manager_v1")
            });
        state.compositor = Some(compositor.clone());
        state.wm_base = Some(wm_base.clone());

        // Phase 1: an xdg-toplevel parent, committed bufferless and
        // round-tripped, so the server has announced and tracked it.
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server announces the toplevel");

        // Phase 2: a square shm buffer for the icon, then an icon carrying both
        // a name and that buffer, assigned to the toplevel. 64x64 ARGB8888
        // needs a 16 KiB backing file.
        const W: i32 = 64;
        const H: i32 = 64;
        const STRIDE: i32 = W * 4;
        let size = STRIDE * H;
        let file = crate::common::create_shm_backing(&shm_path, size as u64);
        let pool = shm.create_pool(file.as_fd(), size, &qh, ());
        let buffer = pool.create_buffer(0, W, H, STRIDE, wl_shm::Format::Argb8888, &qh, ());
        let icon = icon_manager.create_icon(&qh, ());
        icon.set_name("wlr-test-icon".to_owned());
        icon.add_buffer(&buffer, 1);
        icon_manager.set_icon(&toplevel, Some(&icon));
        // The icon is double-buffered: the commit applies it and wlroots emits
        // its `set_icon` during that apply.
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server applies the icon");

        // Phase 2b: the reset. A null icon, applied by the commit that
        // follows, which wlroots forwards as `set_icon` with a null icon.
        icon_manager.set_icon(&toplevel, None);
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server applies the icon reset");

        // Phase 3: the tag and its translated description. wlroots forwards
        // each request immediately rather than at commit.
        if let Some(tag_manager) = tag_manager.as_ref() {
            tag_manager.set_toplevel_tag(&toplevel, "wlr-test-tag".to_owned());
            tag_manager.set_toplevel_description(&toplevel, "WlR test description".to_owned());
            queue
                .roundtrip(&mut state)
                .expect("roundtrip so the server sees the tag and description");

            // Phase 3b: the empty tag and description. Empty strings are legal
            // protocol values, forwarded like any other; the handler must observe
            // them after the non-empty ones.
            tag_manager.set_toplevel_tag(&toplevel, String::new());
            tag_manager.set_toplevel_description(&toplevel, String::new());
            queue
                .roundtrip(&mut state)
                .expect("roundtrip so the server sees the empty tag and description");
        }

        drop((
            icon,
            tag_manager,
            icon_manager,
            buffer,
            pool,
            file,
            surface,
            xdg_surface,
            toplevel,
            shm,
            wm_base,
            compositor,
        ));
        // Best-effort: the backing file is unlinked now that every handle is
        // closed, so a crashed run cannot leave a stale file behind.
        let _ = std::fs::remove_file(&shm_path);
        state.events
    })
}

/// Which single-form icon [`spawn_toplevel_icon_form`] assigns to its
/// toplevel: the protocol allows a name, pixel buffers, or both, and each
/// partial form must reach the handler with the other half absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconForm {
    /// `set_name` only: `name()` reads back, `buffer()` is absent.
    NameOnly,
    /// One 64x64 pixel buffer, no name: `name()` is absent, `buffer()` is
    /// present at that size.
    BufferOnly,
    /// Two pixel buffers (64x64, then 32x32) and no name: `buffer()` returns
    /// the first the client added.
    TwoBuffers,
}

/// Drive an icon carrying exactly one [`IconForm`] on a fresh toplevel: the
/// role is committed bufferless and round-tripped first (so the server has
/// announced it), then the icon is assigned and applied by a second commit.
/// Each phase is round-tripped; the server-side handler assertions are the
/// proof, as for [`spawn_toplevel_meta`].
pub fn spawn_toplevel_icon_form(
    socket: &str,
    form: IconForm,
) -> std::thread::JoinHandle<ClientEvents> {
    let path = crate::common::isolated_runtime_dir().join(socket);
    let shm_path = crate::common::shm_path_for(&format!("{socket}-icon-form"));
    let stream = crate::common::connect_socket(&path);
    std::thread::spawn(move || {
        let (_conn, globals, mut queue, mut state) = ClientState::fresh(stream);
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
        let icon_manager: xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind xdg_toplevel_icon_manager_v1");
        state.compositor = Some(compositor.clone());
        state.wm_base = Some(wm_base.clone());

        // Phase 1: the toplevel, committed bufferless and round-tripped.
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server announces the toplevel");

        // Phase 2: the icon in the requested form. The backing store (when
        // the form needs buffers) is held to the end of the thread so the
        // server never reads a torn-down pool.
        let icon = icon_manager.create_icon(&qh, ());
        let _backing: Option<(
            std::fs::File,
            wl_shm_pool::WlShmPool,
            wl_buffer::WlBuffer,
            Option<wl_buffer::WlBuffer>,
        )> = match form {
            IconForm::NameOnly => {
                icon.set_name("wlr-form-name".to_owned());
                None
            }
            IconForm::BufferOnly => {
                const W: i32 = 64;
                const H: i32 = 64;
                const STRIDE: i32 = W * 4;
                let size = STRIDE * H;
                let file = crate::common::create_shm_backing(&shm_path, size as u64);
                let pool = shm.create_pool(file.as_fd(), size, &qh, ());
                let buffer = pool.create_buffer(0, W, H, STRIDE, wl_shm::Format::Argb8888, &qh, ());
                icon.add_buffer(&buffer, 1);
                Some((file, pool, buffer, None))
            }
            IconForm::TwoBuffers => {
                // Deliberately different sizes so the handler can tell which
                // buffer `buffer()` returned: the first added is 64x64.
                const W1: i32 = 64;
                const H1: i32 = 64;
                const W2: i32 = 32;
                const H2: i32 = 32;
                let size1 = W1 * 4 * H1;
                let size2 = W2 * 4 * H2;
                let file = crate::common::create_shm_backing(&shm_path, (size1 + size2) as u64);
                let pool = shm.create_pool(file.as_fd(), size1 + size2, &qh, ());
                let first =
                    pool.create_buffer(0, W1, H1, W1 * 4, wl_shm::Format::Argb8888, &qh, ());
                let second =
                    pool.create_buffer(size1, W2, H2, W2 * 4, wl_shm::Format::Argb8888, &qh, ());
                icon.add_buffer(&first, 1);
                icon.add_buffer(&second, 2);
                Some((file, pool, first, Some(second)))
            }
        };
        icon_manager.set_icon(&toplevel, Some(&icon));
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server applies the icon");
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server drains");

        drop((
            icon,
            icon_manager,
            _backing,
            surface,
            xdg_surface,
            toplevel,
            shm,
            wm_base,
            compositor,
        ));
        // Best-effort, as for the meta driver; the name-only form created no
        // file, so there is nothing to unlink there.
        let _ = std::fs::remove_file(&shm_path);
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

impl Dispatch<xdg_activation_v1::XdgActivationV1, ()> for ClientState {
    /// Carries no events; bound only so `get_activation_token`/`activate` can
    /// be sent.
    fn event(
        _state: &mut Self,
        _proxy: &xdg_activation_v1::XdgActivationV1,
        _event: xdg_activation_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for ClientState {
    /// `done` carries the token string the server generated; the round trip
    /// stores it so it can be redeemed.
    fn event(
        state: &mut Self,
        _proxy: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.events.activation_token_received = true;
            state.activation_token = Some(token);
        }
    }
}

impl Dispatch<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1, ()> for ClientState {
    /// The `icon_size`/`done` preference events are advisory; the client
    /// ignores them and sets its own icon size.
    fn event(
        _state: &mut Self,
        _proxy: &xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1,
        _event: xdg_toplevel_icon_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_toplevel_icon_v1::XdgToplevelIconV1, ()> for ClientState {
    /// `xdg_toplevel_icon_v1` carries no events.
    fn event(
        _state: &mut Self,
        _proxy: &xdg_toplevel_icon_v1::XdgToplevelIconV1,
        _event: xdg_toplevel_icon_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1, ()> for ClientState {
    /// `xdg_toplevel_tag_manager_v1` carries no events.
    fn event(
        _state: &mut Self,
        _proxy: &xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1,
        _event: xdg_toplevel_tag_manager_v1::Event,
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
    /// `sync_output` carries no test signal. The server either reports the
    /// sample (`presented`, via `send_presented`) or destroys the feedback
    /// (sending `discarded`, via the sampled-then-dropped path); both are
    /// recorded, and the round-trips drain everything.
    fn event(
        state: &mut Self,
        _proxy: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Discarded => {
                state.events.feedback_discarded = true;
            }
            wp_presentation_feedback::Event::Presented { .. } => {
                state.events.feedback_presented = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_tearing_control_manager_v1::WpTearingControlManagerV1, ()> for ClientState {
    /// `wp_tearing_control_manager_v1` carries no events; the Dispatch impl
    /// exists only so [`spawn_mapped_with_tearing`] can bind the global.
    fn event(
        _state: &mut Self,
        _proxy: &wp_tearing_control_manager_v1::WpTearingControlManagerV1,
        _event: wp_tearing_control_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_tearing_control_v1::WpTearingControlV1, ()> for ClientState {
    /// `wp_tearing_control_v1` carries no events; the hint is read
    /// server-side. The Dispatch impl exists only so
    /// [`spawn_mapped_with_tearing`] can create the control object.
    fn event(
        _state: &mut Self,
        _proxy: &wp_tearing_control_v1::WpTearingControlV1,
        _event: wp_tearing_control_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, ()> for ClientState {
    /// The `toplevel` event hands the client its handle for an exported
    /// toplevel; the manager replays every live one on bind.
    fn event(
        state: &mut Self,
        _proxy: &zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state.foreign.toplevels_seen += 1;
            state.foreign_handle = Some(toplevel);
        }
    }

    // The `toplevel` event creates the handle object; its user data is `()`,
    // the `U` of the handle's own `Dispatch` impl below.
    wayland_client::event_created_child!(ClientState, zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (
            zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
            ()
        ),
    ]);
}

impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()> for ClientState {
    /// Records the title/app-id/state the server replayed. `closed` (the
    /// answer to the driven flow's own terminal `close()` request) and
    /// `parent` (one per bind-time replay) are recorded too rather than
    /// swallowed. The client's requests are driven by
    /// [`spawn_foreign_toplevel`].
    fn event(
        state: &mut Self,
        _proxy: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                state.foreign.title = Some(title);
            }
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.foreign.app_id = Some(app_id);
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: _ } => {
                state.foreign.state_events += 1;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                state.foreign.saw_done = true;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.foreign.saw_closed = true;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Parent { .. } => {
                state.foreign.parent_events += 1;
            }
            _ => {}
        }
    }
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for ClientState {
    /// The `toplevel` event hands the client its handle for an exported
    /// toplevel; the list replays every live one on bind.
    fn event(
        state: &mut Self,
        _proxy: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.ext_foreign.toplevels_seen += 1;
            state.ext_foreign_handle = Some(toplevel);
        }
    }

    wayland_client::event_created_child!(ClientState, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (
            ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
            ()
        ),
    ]);
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()> for ClientState {
    /// Records the title/app-id/identifier the list replayed. `closed` is
    /// recorded too: the observed handle is never closed during the run, so a
    /// close would show up here instead of vanishing into `_`.
    fn event(
        state: &mut Self,
        _proxy: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                state.ext_foreign.title = Some(title);
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.ext_foreign.app_id = Some(app_id);
            }
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                state.ext_foreign.identifier = Some(identifier);
            }
            ext_foreign_toplevel_handle_v1::Event::Done => {
                state.ext_foreign.saw_done = true;
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.ext_foreign.saw_closed = true;
            }
            // The generated enum is `non_exhaustive`, so the wildcard stays
            // even with every known variant named.
            _ => {}
        }
    }
}

impl Dispatch<ext_workspace_manager_v1::ExtWorkspaceManagerV1, ()> for ClientState {
    /// `workspace_group` and `workspace` hand the client handles for the
    /// objects the compositor created; the manager replays each live one on
    /// bind, then `done`. Requests are driven by [`spawn_ext_workspace`].
    fn event(
        state: &mut Self,
        _proxy: &ext_workspace_manager_v1::ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state.ext_workspace.groups_seen += 1;
                state.ext_workspace_group = Some(workspace_group);
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state.ext_workspace.workspaces_seen += 1;
                state.ext_workspace_handle = Some(workspace);
            }
            ext_workspace_manager_v1::Event::Done => {
                state.ext_workspace.saw_done = true;
            }
            ext_workspace_manager_v1::Event::Finished => {
                state.ext_workspace.saw_finished = true;
            }
            // The generated enum is `non_exhaustive`, so the wildcard stays
            // even with every known variant named.
            _ => {}
        }
    }

    wayland_client::event_created_child!(ClientState, ext_workspace_manager_v1::ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (
            ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
            ()
        ),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (
            ext_workspace_handle_v1::ExtWorkspaceHandleV1,
            ()
        ),
    ]);
}

impl Dispatch<ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1, ()> for ClientState {
    /// `capabilities`/`output_enter`/`output_leave`/`workspace_enter`/
    /// `workspace_leave` are advisory for this flow; the group is bound only
    /// so [`spawn_ext_workspace`] can call `create_workspace`. `removed` is
    /// recorded: the group is never removed during the run.
    fn event(
        state: &mut Self,
        _proxy: &ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_workspace_group_handle_v1::Event::Removed = event {
            state.ext_workspace.group_removed = true;
        }
    }
}

impl Dispatch<ext_workspace_handle_v1::ExtWorkspaceHandleV1, ()> for ClientState {
    /// `id`/`name`/`coordinates`/`state`/`capabilities` are advisory for this
    /// flow; the workspace is bound only so [`spawn_ext_workspace`] can send
    /// its requests. `removed` is recorded: the workspace is never removed
    /// during the run.
    fn event(
        state: &mut Self,
        _proxy: &ext_workspace_handle_v1::ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_workspace_handle_v1::Event::Removed = event {
            state.ext_workspace.workspace_removed = true;
        }
    }
}

impl Dispatch<wp_security_context_manager_v1::WpSecurityContextManagerV1, ()> for ClientState {
    /// `wp_security_context_manager_v1` carries no events; the Dispatch impl
    /// exists only so [`spawn_security_context`] can bind the global.
    fn event(
        _state: &mut Self,
        _proxy: &wp_security_context_manager_v1::WpSecurityContextManagerV1,
        _event: wp_security_context_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_security_context_v1::WpSecurityContextV1, ()> for ClientState {
    /// `wp_security_context_v1` carries no events; the Dispatch impl exists
    /// only so [`spawn_security_context`] can create and commit the context.
    fn event(
        _state: &mut Self,
        _proxy: &wp_security_context_v1::WpSecurityContextV1,
        _event: wp_security_context_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for ClientState {
    /// `zwlr_layer_shell_v1` carries no events; the Dispatch impl exists only
    /// so [`spawn_layer_surface`] can bind the global.
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for ClientState {
    /// Records and acks the initial `configure`, and records `closed`. The
    /// configure/ack counters reuse [`ClientEvents`]' existing fields so a
    /// caller asserts the same wire facts a toplevel client does.
    fn event(
        state: &mut Self,
        proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                state.events.configure_events += 1;
                proxy.ack_configure(serial);
                state.events.acked_configures += 1;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.events.layer_closed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for ClientState {
    /// `wl_output` events are irrelevant to the lock-surface flow; the
    /// Dispatch impl exists only so [`spawn_session_lock`] can bind the
    /// output the lock surface covers.
    fn event(
        _state: &mut Self,
        _proxy: &wl_output::WlOutput,
        _event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_session_lock_manager_v1::ExtSessionLockManagerV1, ()> for ClientState {
    /// `ext_session_lock_manager_v1` carries no events; the Dispatch impl
    /// exists only so [`spawn_session_lock`] can bind the global.
    fn event(
        _state: &mut Self,
        _proxy: &ext_session_lock_manager_v1::ExtSessionLockManagerV1,
        _event: ext_session_lock_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_session_lock_v1::ExtSessionLockV1, ()> for ClientState {
    /// Records `locked` and `finished`. The driver never unlocks, so
    /// `finished` must stay false; `locked` proves the server mapped the
    /// covering surface.
    fn event(
        state: &mut Self,
        _proxy: &ext_session_lock_v1::ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => {
                state.session_lock.saw_locked = true;
            }
            ext_session_lock_v1::Event::Finished => {
                state.session_lock.saw_finished = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, ()> for ClientState {
    /// Records the last `configure` (serial, width, height). The ack is
    /// driven by [`spawn_session_lock`] between the round-trips — not here —
    /// so the acked serial is known separately from a later re-configure.
    fn event(
        state: &mut Self,
        _proxy: &ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            state.session_lock.configure = Some((serial, width, height));
        }
    }
}
