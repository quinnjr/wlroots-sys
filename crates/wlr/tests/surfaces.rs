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
//!
//! The same client run is the only place the `wlr_surface_*` operation
//! surface is reachable at all: it is what turns a zeroed scratch allocation
//! into a live, committed, mapped `wlr_surface` with a real output and seat
//! behind it. Every accessor and mutator added in this milestone is called
//! from inside `surface_committed` (or the run's `should_stop`) so the whole
//! list runs under the harness's real memory.

mod common;

use std::thread::JoinHandle;
use std::time::Duration;

use wlr::{Backend, Box2D, Display, FBox, OutputId, Runtime, SurfaceId, Transform, Until};

/// Everything the client-driven run observed through the new `Surface`
/// operations. Fields are vectors rather than single values because
/// `surface_committed` fires once per commit.
#[derive(Default)]
struct Probe {
    committed: Vec<SurfaceId>,
    mapped: Vec<SurfaceId>,
    unmapped: Vec<SurfaceId>,
    destroyed: Vec<SurfaceId>,
    sizes: Vec<(i32, i32)>,
    extents: Vec<Box2D>,
    source_boxes: Vec<FBox>,
    damage_boxes: Vec<Box2D>,
    hits: Vec<Option<SurfaceId>>,
    root_id_always_self: bool,
    point_accepts_input: Option<bool>,
    accepts_touch: Option<bool>,
    as_layer_always_none: bool,
    locked_once: bool,
    unmapped_once: bool,
    /// Whether the same-surface `unlock_cached` succeeded. Recorded rather
    /// than asserted in the handler (a panic there would abort through C).
    unlock_ok: Option<bool>,
    /// Far-outside hit-test: `surface_at` at a point no surface covers.
    miss_at: Option<bool>,
    /// Far-outside input check at the same point.
    miss_accepts_input: Option<bool>,
    /// `Surface::as_toplevel` on the live toplevel surface: every commit must
    /// resolve, and the resolved id must name the same surface.
    as_toplevel_matched: Vec<bool>,
}

impl Probe {
    fn new() -> Probe {
        Probe {
            root_id_always_self: true,
            as_layer_always_none: true,
            ..Probe::default()
        }
    }
}

struct App {
    toplevels: usize,
    probe: Probe,
    /// The client thread, owned here so [`LoopHandler::should_stop`] can end
    /// the single `Until::Stop` run once the client is done. A per-turn
    /// `Until::Turns` loop would tear the announcing session down between the
    /// client's requests, and a `SurfaceId` is only good for the run that
    /// announced it.
    client: Option<JoinHandle<common::client::ClientEvents>>,
    runtime: Runtime,
    output: Option<OutputId>,
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        let _ = output.enable_with_preferred_mode();
        let _ = self.runtime.init_output(output);
        self.output = Some(output.id());
    }
}

impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, _t: &wlr::Toplevel<'_>) {
        self.toplevels += 1;
    }

    fn surface_committed(&mut self, surface: &wlr::Surface<'_>) {
        // Exercise the handle path: a commit is where the surface's state is
        // worth reading, and the id must survive the call.
        let id = surface.id();
        self.probe.sizes.push(surface.current_size());
        self.probe.committed.push(id);

        self.probe.extents.push(surface.extents());
        self.probe.source_boxes.push(surface.buffer_source_box());
        self.probe
            .damage_boxes
            .push(surface.effective_damage().extents());
        self.probe.root_id_always_self &= surface.root_id() == id;
        self.probe.point_accepts_input = Some(surface.point_accepts_input(1.0, 1.0));
        self.probe
            .hits
            .push(surface.surface_at(1.0, 1.0).map(|(leaf, _, _)| leaf.id()));
        self.probe.accepts_touch = Some(surface.accepts_touch());
        self.probe.as_layer_always_none &= surface.as_layer_surface().is_none();
        // The thin toplevel downcast: this surface is a live xdg-toplevel, so
        // both the handle downcast and the by-id downcast must resolve.
        self.probe
            .as_toplevel_matched
            .push(surface.as_toplevel().is_some() && self.runtime.toplevel_of(id).is_some());
        // A point far outside any surface misses cleanly rather than
        // hit-testing to something or claiming input.
        self.probe.miss_at = Some(surface.surface_at(1e6, 1e6).is_none());
        self.probe.miss_accepts_input = Some(surface.point_accepts_input(1e6, 1e6));

        // Mutators that send the client nothing observable (or, for the
        // preferred scale/transform, an event the client ignores here).
        let _ = surface.set_preferred_buffer_scale(2);
        surface.set_preferred_buffer_transform(Transform::Normal);
        surface.send_frame_done(Duration::ZERO);
        if let Some(output) = self.output.and_then(|o| self.runtime.output(o)) {
            surface.send_enter(&output);
            surface.send_leave(&output);
        }

        // Lock then immediately release the pending state, once. The pair is
        // what wlroots' own API requires; doing it twice would defer a second
        // time for no added coverage.
        if !self.probe.locked_once {
            self.probe.locked_once = true;
            let lock = surface.lock_pending();
            self.probe.unlock_ok = Some(surface.unlock_cached(lock).is_ok());
        }

        // Unmap once, after the surface is mapped. The queued
        // `surface_unmapped` event is what the assertion below reads.
        if surface.mapped() && !self.probe.unmapped_once {
            self.probe.unmapped_once = true;
            surface.unmap();
        }
    }

    fn surface_mapped(&mut self, id: SurfaceId) {
        self.probe.mapped.push(id);
    }

    fn surface_unmapped(&mut self, id: SurfaceId) {
        self.probe.unmapped.push(id);
    }

    fn surface_destroyed(&mut self, id: SurfaceId) {
        self.probe.destroyed.push(id);
    }
}

impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

fn app(runtime: &Runtime) -> App {
    App {
        toplevels: 0,
        probe: Probe::new(),
        client: None,
        runtime: runtime.clone(),
        output: None,
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
    let mut app = app(&runtime);
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    assert!(runtime.surface(SurfaceId::dangling_for_test()).is_none());
}

/// A real client creates a toplevel, commits bufferless, acks the configure,
/// commits a buffer that maps it, and then disconnects. The server must observe
/// the generic surface lifecycle for that surface — `surface_committed` (with a
/// live [`wlr::Surface`] handle), `surface_mapped`, and `surface_destroyed` —
/// all naming the same id, and every `wlr_surface_*` operation reached from the
/// handler must run against that live surface without faulting.
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
    runtime.create_seat(&display, "test-seat").expect("seat");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = app(&runtime);
    app.client = Some(common::client::spawn_mapped(&socket));

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
    let probe = &app.probe;

    assert_eq!(
        app.toplevels, 1,
        "the server should observe exactly one toplevel from the client"
    );
    assert!(
        events.configure_events >= 1 && events.acked_configures >= 1,
        "the client's configure should have been acked before the buffered commit"
    );
    assert!(
        !probe.committed.is_empty(),
        "the generic commit event must reach the handler for the client's surface"
    );
    assert!(
        !probe.mapped.is_empty(),
        "the buffered commit must map the surface and deliver surface_mapped"
    );
    assert_eq!(
        probe.mapped.len(),
        1,
        "the surface maps exactly once on this path"
    );
    let mapped = probe.mapped[0];
    assert!(
        probe.committed.contains(&mapped),
        "commit and map name the same surface id"
    );
    assert!(
        probe.destroyed.contains(&mapped),
        "the client's disconnect must destroy the surface and deliver surface_destroyed"
    );
    assert!(
        probe.sizes.iter().all(|(w, h)| *w >= 0 && *h >= 0),
        "surface_committed read a live handle's size without panicking"
    );

    // wlr_surface_* accessors.
    assert!(
        probe.root_id_always_self,
        "wlr_surface_get_root_surface of a mapped toplevel is itself"
    );
    assert!(
        probe.extents.iter().any(|b| b.width >= 0 && b.height >= 0),
        "extents returned a real box"
    );
    assert!(
        probe.source_boxes.iter().any(|b| !b.empty()),
        "the buffered commit yielded a non-empty buffer source box"
    );
    assert!(
        probe
            .damage_boxes
            .iter()
            .any(|b| b.width >= 0 && b.height >= 0),
        "effective damage returned a real region"
    );
    assert!(
        probe.hits.iter().any(|hit| hit.is_some()),
        "a point inside the mapped surface hit-tests to a surface"
    );
    assert_eq!(
        probe.point_accepts_input,
        Some(true),
        "the default input region of a mapped surface accepts a point inside it"
    );
    assert_eq!(
        probe.accepts_touch,
        Some(false),
        "the client bound no touch device, so the surface accepts no touch"
    );
    assert!(
        probe.as_layer_always_none,
        "a toplevel is not a layer surface"
    );

    // wlr_surface_unmap, observed through the generic unmap event.
    assert!(
        probe.unmapped.contains(&mapped),
        "wlr_surface_unmap must unmap the mapped surface and deliver surface_unmapped"
    );

    // The same-surface lock/unlock pair must succeed; the cross-surface `Err`
    // refusal (unlocking with another surface's token hands it back) stays
    // covered by the `surface.rs` unit test, not this single-surface run.
    assert_eq!(
        probe.unlock_ok,
        Some(true),
        "unlock_cached must succeed for the lock the same surface minted"
    );

    // Far-outside hit-testing misses cleanly.
    assert_eq!(
        probe.miss_at,
        Some(true),
        "surface_at at a far-outside point must miss rather than hit-test to a surface"
    );
    assert_eq!(
        probe.miss_accepts_input,
        Some(false),
        "point_accepts_input at a far-outside point must be false"
    );
    assert!(
        !probe.as_toplevel_matched.is_empty() && probe.as_toplevel_matched.iter().all(|m| *m),
        "Surface::as_toplevel (and Runtime::toplevel_of) must resolve every \
         commit of the live toplevel surface"
    );
}
