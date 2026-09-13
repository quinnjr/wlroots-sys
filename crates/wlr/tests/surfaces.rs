//! The generic surface model, against a real headless compositor.
//!
//! Two positive paths and one negative. A real Wayland client drives a
//! toplevel through bufferless commit, configure ack, buffered commit (map)
//! and disconnect (destroy), and the server must observe the generic
//! `surface_committed`/`surface_mapped`/`surface_destroyed` events for the
//! surface it created; a dangling id must miss rather than dereference. The
//! direct signal-level link/emit/unlink proof lives in `backend.rs`'s
//! `generic_surface_listeners_link_deliver_and_unlink`, which can emit the
//! exact wlroots signals without a client.

mod common;

use std::thread::JoinHandle;

use wlr::{Backend, Display, Runtime, SurfaceId, Until};

#[derive(Default)]
struct App {
    toplevels: usize,
    committed: Vec<SurfaceId>,
    mapped: Vec<SurfaceId>,
    destroyed: Vec<SurfaceId>,
    sizes: Vec<(i32, i32)>,
    /// The client thread, owned here so [`LoopHandler::should_stop`] can end
    /// the single `Until::Stop` run once the client is done. A per-turn
    /// `Until::Turns` loop would tear the announcing session down between the
    /// client's requests, and a `SurfaceId` is only good for the run that
    /// announced it.
    client: Option<JoinHandle<common::client::ClientEvents>>,
}

impl wlr::OutputHandler for App {}
impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, _t: &wlr::Toplevel<'_>) {
        self.toplevels += 1;
    }

    fn surface_committed(&mut self, surface: &wlr::Surface<'_>) {
        // Exercise the handle path too: a commit is where the surface's size
        // is worth reading, and the id must survive the call.
        self.sizes.push(surface.current_size());
        self.committed.push(surface.id());
    }

    fn surface_mapped(&mut self, id: SurfaceId) {
        self.mapped.push(id);
    }

    fn surface_destroyed(&mut self, id: SurfaceId) {
        self.destroyed.push(id);
    }
}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

#[test]
fn runtime_surface_misses_on_a_dangling_id() {
    common::headless_env();
    let _serial = common::headless_guard();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    let mut app = App::default();
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    assert!(runtime.surface(SurfaceId::dangling_for_test()).is_none());
}

/// A real client creates a toplevel, commits bufferless, acks the configure,
/// commits a buffer that maps it, and then disconnects. The server must observe
/// the generic surface lifecycle for that surface — `surface_committed` (with a
/// live [`wlr::Surface`] handle), `surface_mapped`, and `surface_destroyed` —
/// all naming the same id.
#[test]
fn a_real_client_surface_is_committed_mapped_and_destroyed() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir(); // XDG_RUNTIME_DIR must exist before the socket is bound
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        client: Some(common::client::spawn_mapped(&socket)),
        ..App::default()
    };

    // One run, so the generic listeners installed when the client's toplevel is
    // announced stay linked through the buffered commit that maps it and the
    // disconnect that destroys it. `Until::Stop` blocks between turns and the
    // client's own traffic — and finally its disconnect — is what ends the run
    // through `should_stop`; the client socket carries a 10s timeout, so a
    // stuck client turns into a finished thread rather than an infinite block.
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
        app.toplevels, 1,
        "the server should observe exactly one toplevel from the client"
    );
    assert!(
        events.configure_events >= 1 && events.acked_configures >= 1,
        "the client's configure should have been acked before the buffered commit"
    );
    assert!(
        !app.committed.is_empty(),
        "the generic commit event must reach the handler for the client's surface"
    );
    assert!(
        !app.mapped.is_empty(),
        "the buffered commit must map the surface and deliver surface_mapped"
    );
    assert_eq!(
        app.mapped.len(),
        1,
        "the surface maps exactly once on this path"
    );
    let mapped = app.mapped[0];
    assert!(
        app.committed.contains(&mapped),
        "commit and map name the same surface id"
    );
    assert!(
        app.destroyed.contains(&mapped),
        "the client's disconnect must destroy the surface and deliver surface_destroyed"
    );
    assert!(
        app.sizes.iter().all(|(w, h)| *w >= 0 && *h >= 0),
        "surface_committed read a live handle's size without panicking"
    );
}
