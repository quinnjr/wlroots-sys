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
    /// Per-commit `point_accepts_input` answers, so every commit's input
    /// region is witnessed rather than just the last one's.
    point_accepts_input: Vec<bool>,
    /// Per-commit `accepts_touch` answers, for the same reason.
    accepts_touch: Vec<bool>,
    as_layer_always_none: bool,
    locked_once: bool,
    unmapped_once: bool,
    /// Whether the same-surface `unlock_cached` succeeded. Recorded rather
    /// than asserted in the handler (a panic there would abort through C).
    unlock_ok: Vec<bool>,
    /// Far-outside hit-test: `surface_at` at a point no surface covers.
    miss_at: Vec<bool>,
    /// Far-outside input check at the same point.
    miss_accepts_input: Vec<bool>,
    /// Per-commit `(committing id, visited ids)` from `for_each_surface`, so
    /// the walk is witnessed on a live tree rather than just a scratch one.
    for_each_visited: Vec<(SurfaceId, Vec<SurfaceId>)>,
    /// `(parent, child)` pairs from `new_subsurface`, for the sub-surface
    /// run below.
    subsurface_pairs: Vec<(SurfaceId, SurfaceId)>,
    /// `(child, child.root_id(), parent.root_id())` read live during
    /// `new_subsurface`, while both surfaces are known alive.
    subsurface_roots: Vec<(SurfaceId, SurfaceId, SurfaceId)>,
    /// `Surface::as_toplevel` on the live toplevel surface: every commit must
    /// resolve, and the resolved id must name the same surface.
    as_toplevel_matched: Vec<bool>,
    /// `Runtime::surface` at each `surface_mapped`: the mapped id must
    /// resolve while its run is live, so the post-run miss below proves
    /// staleness rather than a surface that never resolved.
    mapped_live: Vec<bool>,
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
        self.probe
            .point_accepts_input
            .push(surface.point_accepts_input(1.0, 1.0));
        self.probe
            .hits
            .push(surface.surface_at(1.0, 1.0).map(|(leaf, _, _)| leaf.id()));
        self.probe.accepts_touch.push(surface.accepts_touch());
        self.probe.as_layer_always_none &= surface.as_layer_surface().is_none();
        // The thin toplevel downcast: this surface is a live xdg-toplevel, so
        // both the handle downcast and the by-id downcast must resolve.
        self.probe
            .as_toplevel_matched
            .push(surface.as_toplevel().is_some() && self.runtime.toplevel_of(id).is_some());
        // A point far outside any surface misses cleanly rather than
        // hit-testing to something or claiming input.
        self.probe
            .miss_at
            .push(surface.surface_at(1e6, 1e6).is_none());
        self.probe
            .miss_accepts_input
            .push(surface.point_accepts_input(1e6, 1e6));

        // The tree walk visits the committing surface itself: on this run the
        // tree is a single root, so the walk must yield exactly it.
        let mut visited = Vec::new();
        surface.for_each_surface(|leaf, _, _| visited.push(leaf.id()));
        self.probe.for_each_visited.push((id, visited));

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
            self.probe
                .unlock_ok
                .push(surface.unlock_cached(lock).is_ok());
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
        self.probe
            .mapped_live
            .push(self.runtime.surface(id).is_some());
    }

    fn surface_unmapped(&mut self, id: SurfaceId) {
        self.probe.unmapped.push(id);
    }

    fn surface_destroyed(&mut self, id: SurfaceId) {
        self.probe.destroyed.push(id);
    }

    fn new_subsurface(&mut self, parent: SurfaceId, child: SurfaceId) {
        self.probe.subsurface_pairs.push((parent, child));
        // Both surfaces are alive here: the child just joined the parent's
        // state, so resolving the child's foreign root is the no-fabrication
        // witness — it must name the tracked parent, not an invented id.
        if let (Some(parent_surface), Some(child_surface)) =
            (self.runtime.surface(parent), self.runtime.surface(child))
        {
            self.probe.subsurface_roots.push((
                child,
                child_surface.root_id(),
                parent_surface.root_id(),
            ));
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
    assert!(
        !probe.point_accepts_input.is_empty() && probe.point_accepts_input.iter().any(|v| *v),
        "the default input region of the mapped surface accepts a point inside it; \
         early bufferless commits legitimately report false"
    );
    assert!(
        !probe.accepts_touch.is_empty() && probe.accepts_touch.iter().all(|v| !v),
        "the client bound no touch device, so the surface accepts no touch on every commit"
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
        vec![true],
        "unlock_cached must succeed for the lock the same surface minted"
    );

    // Far-outside hit-testing misses cleanly.
    assert!(
        !probe.miss_at.is_empty() && probe.miss_at.iter().all(|v| *v),
        "surface_at at a far-outside point must miss rather than hit-test to a surface"
    );
    assert!(
        !probe.miss_accepts_input.is_empty() && probe.miss_accepts_input.iter().all(|v| !v),
        "point_accepts_input at a far-outside point must be false"
    );
    assert!(
        !probe.as_toplevel_matched.is_empty() && probe.as_toplevel_matched.iter().all(|m| *m),
        "Surface::as_toplevel (and Runtime::toplevel_of) must resolve every \
         commit of the live toplevel surface"
    );

    // The tree walk runs on a live tree with no sub-surfaces: every commit
    // must visit exactly the committing surface itself.
    assert_eq!(
        probe.for_each_visited.len(),
        probe.committed.len(),
        "for_each_surface must run once per commit"
    );
    for (id, visited) in &probe.for_each_visited {
        assert_eq!(
            visited,
            &vec![*id],
            "the walk over a childless live tree visits exactly its root"
        );
    }
    assert!(
        !probe.mapped_live.is_empty() && probe.mapped_live.iter().all(|v| *v),
        "the mapped id resolved while its run was live"
    );
    // Surface tables are per-run — the same rule output_layout.rs's
    // `layout_box_after_the_run_is_stale_and_misses_cleanly` pins for
    // outputs — so the mapped id, announced by this run, is stale now that
    // `run_all` has returned and must miss rather than resolve freed memory.
    assert!(
        runtime.surface(mapped).is_none(),
        "a SurfaceId kept past its run must miss"
    );
}

/// A mapped child sub-surface resolves its foreign root to the tracked
/// parent — not to an invented id — and the parent's tree walk yields it.
///
/// A foreign *untracked* root has no closer witness than this: every surface
/// wlroots announces goes through `install_surface_listeners`, so a live
/// root is always tracked, and a root for a scratch handle is always the
/// handle itself (pinned by `root_id_of_a_roleless_surface_is_its_own_id`).
/// The fabrication this guards against would show up here as the child
/// resolving anywhere but the parent.
#[test]
fn a_mapped_subsurface_resolves_its_foreign_root_and_is_walked() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    runtime.create_seat(&display, "test-seat").expect("seat");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = app(&runtime);
    app.client = Some(common::client::spawn_subsurface_mapped(&socket));

    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");

    let _ = app
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
    assert_eq!(
        probe.subsurface_pairs.len(),
        1,
        "the server should observe exactly one new_subsurface from the client"
    );
    let (parent, child) = probe.subsurface_pairs[0];
    assert_ne!(parent, child, "parent and child are distinct surfaces");
    assert_eq!(
        probe.subsurface_roots,
        vec![(child, parent, parent)],
        "the child's foreign root must resolve to the tracked parent, \
         and the parent must resolve to itself — never to an invented id"
    );
    assert!(
        probe
            .for_each_visited
            .iter()
            .any(|(id, visited)| *id == parent
                && visited.contains(&parent)
                && visited.contains(&child)),
        "the parent's tree walk must yield the mapped child alongside the root"
    );
    for (id, visited) in &probe.for_each_visited {
        assert!(
            visited.contains(id),
            "every walk visits its own committing root, got {visited:?} for {id:?}"
        );
    }
}
