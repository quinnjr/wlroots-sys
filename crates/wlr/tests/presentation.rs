//! Per-surface presentation feedback and tearing hints, against a real headless
//! compositor.
//!
//! The by-id miss contract is the memory-safety boundary: an unknown
//! [`SurfaceId`] must resolve to nothing rather than dereference. Against a
//! **live** surface the wrappers are exercised end to end — `Surface::sampled`
//! inside the generic commit handler, and `Surface::textured_on_output` /
//! `Surface::scanned_out_on_output` against a real headless output — proving the
//! `Runtime::surface`/`backend::with_surface` → `with_tearing_manager` plumbing
//! and the FFI calls themselves, not just their scratch unit-test doubles.
//!
//! A real `wp_presentation` feedback object also needs the client to bind
//! `wp_presentation` and ask for feedback; `common::client::spawn_mapped` does
//! that, so `sampled()` takes a genuinely-present feedback on the first commit.
//! The tearing-control legs reuse the same shared driver through
//! [`common::client::spawn_mapped_with_tearing`](TearingSetup): creating the
//! control object (and optionally the async hint) before the first commit is
//! the driver's one parameter, not a fork of it. Handler observations are
//! recorded on the app and asserted after the run, never inside a handler,
//! where a panic would abort through C.

use wlr::{
    Backend, Display, Output, OutputId, PresentEvent, PresentFlags, PresentationEvent,
    PresentationFeedback, Runtime, Surface, SurfaceId, TearingHint, Until,
};

mod common;

/// Records what a live surface exposes through the M9 wrappers.
///
/// Holds a `Runtime` clone because [`Surface::textured_on_output`] needs an
/// [`Output`] handle and the only by-id route to one is
/// [`Runtime::output`]; the runtime the run uses is cloned in so the handler can
/// resolve the output id captured in `new_output`.
struct App {
    runtime: Runtime,
    /// The headless output announced before the client connects.
    output: Option<OutputId>,
    committed: usize,
    /// Commits where `sampled()` returned a feedback (the client asked).
    sampled_some: usize,
    /// Commits where `sampled()` returned nothing: the client's single
    /// feedback request is take-once, so later commits miss.
    sampled_none: usize,
    /// Commits where `tearing_hint()` reported the vsync default.
    hinted_vsync: usize,
    /// Commits where `tearing_hint()` reported the async hint.
    hinted_async: usize,
    /// Commits where `tearing_control()` correctly missed.
    control_missing: usize,
    /// Commits where a client-created tearing control resolved and named
    /// this very surface through `surface_id()`.
    control_surface_matched: usize,
    /// One `(current, pending, previous)` triple per commit where a control
    /// object resolved; the async leg asserts its transitions on these.
    hint_triples: Vec<(TearingHint, TearingHint, TearingHint)>,
    /// Commits where `textured_on_output`/`scanned_out_on_output` ran with a
    /// live output.
    textured: usize,
    scanned: usize,
    /// When set, the handler resolves everything through stored ids — the
    /// by-id battery — instead of the direct handle calls above.
    by_id: bool,
    /// The first surface id the handler saw, captured for the by-id asserts.
    first_surface: Option<SurfaceId>,
    /// Commits where `sample_presentation(id)` resolved a live feedback.
    by_id_sampled_some: usize,
    /// Commits where the by-id hint read the vsync default.
    by_id_hinted_vsync: usize,
    /// Commits where the by-id control lookup resolved an object.
    by_id_control_some: usize,
    /// When set, the first sampled feedback is reported with
    /// `send_presented` instead of being dropped.
    send_presented: bool,
    /// How many sampled feedbacks were reported with `send_presented`.
    presented_sent: usize,
    /// The sent event's fields, read back off the owned event for the
    /// round-trip assert: the output id, the `(tv_sec, tv_nsec)` timestamp,
    /// the refresh, the sequence, and the flags.
    presented_output: Option<OutputId>,
    presented_time: Option<(u64, u32)>,
    presented_refresh: Option<u32>,
    presented_seq: Option<u64>,
    presented_flags: Option<PresentFlags>,
    client: Option<std::thread::JoinHandle<common::client::ClientEvents>>,
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &Output<'_>) {
        self.output = Some(output.id());
    }
}

impl App {
    /// The sampling concern: take the surface's feedback when the client
    /// asked for one, reporting or dropping it per the test's mode.
    ///
    /// With the client's `wp_presentation.feedback` request applied, the
    /// first commit returns `Some`; the wrapper detaches it and dropping it
    /// sends `discarded`. Later commits miss: the single client request is
    /// take-once. In by-id mode the same take happens through the stored id
    /// instead, and the direct call is skipped — it could only miss after
    /// the by-id lookup detached the feedback.
    fn observe_sampling(&mut self, surface: &Surface<'_>) {
        if self.by_id {
            let id = surface.id();
            self.first_surface.get_or_insert(id);
            if self.runtime.sample_presentation(id).is_some() {
                self.by_id_sampled_some += 1;
            }
            return;
        }
        if let Some(feedback) = surface.sampled() {
            self.sampled_some += 1;
            if self.send_presented && self.presented_sent == 0 {
                self.send_first_presented(feedback, surface);
            }
            // Otherwise the feedback drops here, which destroys it
            // server-side and sends `discarded` to the listening client.
        } else {
            self.sampled_none += 1;
        }
    }

    /// Report the first sampled feedback as presented, from a real
    /// [`PresentEvent`] resolved against the live output.
    ///
    /// The event's fields are recorded for the post-run round-trip assert;
    /// the client observes `presented` on a later round-trip.
    fn send_first_presented(&mut self, feedback: PresentationFeedback, surface: &Surface<'_>) {
        let _ = surface;
        let Some(output_id) = self.output else {
            debug_assert!(false, "no output announced before the first sampled commit");
            return;
        };
        let Some(output) = self.runtime.output(output_id) else {
            debug_assert!(false, "the announced output no longer resolves");
            return;
        };
        let present = PresentEvent {
            commit_seq: 1,
            presented: true,
            when: std::time::Duration::new(9, 8),
            seq: 7,
            refresh: 60_000,
            flags: PresentFlags::VSYNC,
        };
        let event = PresentationEvent::from_output(&output, &present);
        self.presented_output = event.output_id();
        self.presented_time = Some((event.tv_sec(), event.tv_nsec()));
        self.presented_refresh = Some(event.refresh());
        self.presented_seq = Some(event.seq());
        self.presented_flags = Some(event.flags());
        feedback.send_presented(&event);
        self.presented_sent += 1;
    }

    /// The tearing concern: read the effective hint, the control object, and
    /// — through the stored id in by-id mode — the by-id twins.
    ///
    /// The manager was cached onto the handle by `with_surface`; a surface
    /// with no client-created control reports the wlroots default. A
    /// client-created control names the surface it was created for, so the
    /// id-addon lookup must resolve to the committing surface's own id.
    fn observe_tearing(&mut self, surface: &Surface<'_>) {
        if surface.tearing_hint() == Some(TearingHint::Vsync) {
            self.hinted_vsync += 1;
        }
        if surface.tearing_hint() == Some(TearingHint::Async) {
            self.hinted_async += 1;
        }
        if surface.tearing_control().is_none() {
            self.control_missing += 1;
        }
        if surface
            .tearing_control()
            .is_some_and(|control| control.surface_id() == Some(surface.id()))
        {
            self.control_surface_matched += 1;
        }
        if self.by_id {
            let id = surface.id();
            self.first_surface.get_or_insert(id);
            if self.runtime.tearing_hint(id) == Some(TearingHint::Vsync) {
                self.by_id_hinted_vsync += 1;
            }
            if self.runtime.tearing_control(id).is_some() {
                self.by_id_control_some += 1;
            }
        }
        if let Some(control) = surface.tearing_control() {
            self.hint_triples.push((
                control.current_hint(),
                control.pending_hint(),
                control.previous_hint(),
            ));
        }
    }

    /// The output concern: run both output-taking presentation wrappers
    /// against the announced output, resolved to a live handle.
    fn run_output_wrappers(&mut self, surface: &Surface<'_>) {
        if let Some(output_id) = self.output
            && let Some(output) = self.runtime.output(output_id)
        {
            surface.textured_on_output(&output);
            self.textured += 1;
            surface.scanned_out_on_output(&output);
            self.scanned += 1;
        }
    }
}

impl wlr::ToplevelHandler for App {
    fn surface_committed(&mut self, surface: &Surface<'_>) {
        self.committed += 1;
        self.observe_sampling(surface);
        self.observe_tearing(surface);
        self.run_output_wrappers(surface);
    }
}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

/// A fresh `App` over `runtime` with every counter at zero and both modes off.
fn new_app(runtime: &Runtime) -> App {
    App {
        runtime: runtime.clone(),
        output: None,
        committed: 0,
        sampled_some: 0,
        sampled_none: 0,
        hinted_vsync: 0,
        hinted_async: 0,
        control_missing: 0,
        control_surface_matched: 0,
        hint_triples: Vec::new(),
        textured: 0,
        scanned: 0,
        by_id: false,
        first_surface: None,
        by_id_sampled_some: 0,
        by_id_hinted_vsync: 0,
        by_id_control_some: 0,
        send_presented: false,
        presented_sent: 0,
        presented_output: None,
        presented_time: None,
        presented_refresh: None,
        presented_seq: None,
        presented_flags: None,
        client: None,
    }
}

/// Every new per-surface operation misses cleanly on an unknown id, before and
/// after the globals exist. Removing the id-table lookup (dereferencing the id as
/// a pointer) would segfault here rather than return `None`.
#[test]
fn surface_presentation_and_tearing_ops_miss_cleanly_without_a_surface() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    // Before either global exists: no surface can have requested feedback or
    // created a tearing control, so every lookup is a miss.
    assert!(
        runtime
            .sample_presentation(SurfaceId::dangling_for_test())
            .is_none(),
        "a dangling surface id has no presentation feedback"
    );
    assert!(
        runtime
            .tearing_hint(SurfaceId::dangling_for_test())
            .is_none(),
        "a dangling surface id has no tearing hint"
    );
    assert!(
        runtime
            .tearing_control(SurfaceId::dangling_for_test())
            .is_none(),
        "a dangling surface id has no tearing control object"
    );

    runtime
        .create_presentation(&display, &backend)
        .expect("presentation creates");
    runtime
        .create_tearing_control_manager(&display, 1)
        .expect("tearing control creates");

    // The globals now exist, but a dangling id still names no surface, so the
    // miss is unchanged — the lookup resolves the id before it reaches wlroots.
    assert!(
        runtime
            .sample_presentation(SurfaceId::dangling_nth_for_test(1))
            .is_none(),
        "the presentation global alone does not make a stale id resolve"
    );
    assert!(
        runtime
            .tearing_hint(SurfaceId::dangling_nth_for_test(1))
            .is_none(),
        "the tearing global alone does not make a stale id resolve"
    );
    assert!(
        runtime
            .tearing_control(SurfaceId::dangling_nth_for_test(1))
            .is_none(),
        "the tearing global alone does not make a stale id resolve"
    );
}

/// A real client maps a surface while the presentation and tearing globals are
/// live. The server must run every new wrapper against that surface: `sampled()`
/// reaches the client's requested feedback, the tearing hint reads the vsync
/// default through the cached manager, and textured/scanned run against the live
/// output.
#[test]
fn a_real_surface_runs_the_presentation_and_tearing_wrappers() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir(); // XDG_RUNTIME_DIR must exist before the socket is bound
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    runtime
        .create_presentation(&display, &backend)
        .expect("presentation");
    runtime
        .create_tearing_control_manager(&display, 1)
        .expect("tearing control");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = new_app(&runtime);
    app.client = Some(common::client::spawn_mapped(&socket));

    // One run, so the output and the client's surface stay live together; the
    // client's disconnect ends it through `should_stop`.
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
        "the client's configure should have been acked before the map"
    );
    assert!(
        events.feedback_discarded,
        "the server's commit handler sampled the requested feedback and dropped it, \
         which destroys it server-side and sends `discarded` to the listening client"
    );
    assert!(app.output.is_some(), "the headless output was announced");
    assert!(
        app.committed >= 1,
        "the client commits at least once and commits reach surface_committed"
    );
    assert_eq!(
        app.textured, app.committed,
        "textured_on_output ran for every commit with a live output"
    );
    assert_eq!(
        app.scanned, app.committed,
        "scanned_out_on_output ran for every commit with a live output"
    );
    assert!(
        app.sampled_some >= 1,
        "the client's wp_presentation.feedback request must reach Surface::sampled"
    );
    assert!(
        app.sampled_none >= 1,
        "sampled() is take-once: commits after the single client request is consumed must return None"
    );
    assert!(
        app.hinted_vsync >= 1,
        "the tearing manager cached by with_surface must make tearing_hint report the default"
    );
    assert!(
        app.control_missing >= 1,
        "no client created a tearing control, so the object lookup must miss"
    );
}

/// A client creates a tearing-control object for its toplevel surface; the
/// server must resolve it through `Surface::tearing_control`, and its
/// `surface_id()` must name the committing surface — the id-addon positive
/// that the miss-only tests cannot cover.
#[test]
fn a_client_created_tearing_control_names_its_surface() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    runtime
        .create_tearing_control_manager(&display, 1)
        .expect("tearing control");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = new_app(&runtime);
    app.client = Some(common::client::spawn_mapped_with_tearing(
        &socket,
        common::client::TearingSetup::Control,
    ));
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert!(
        app.committed >= 1,
        "the client commits at least once and commits reach surface_committed"
    );
    assert!(
        app.control_surface_matched >= 1,
        "tearing_control() resolved on a commit and surface_id() named that surface"
    );
}

/// The by-id battery against a live surface: the handler captures the
/// surface's id on the first commit and resolves everything a compositor
/// would need outside a handler — the sampled feedback, the vsync hint, and
/// the client-created control — through the stored id.
#[test]
fn by_id_lookups_resolve_a_live_surface() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    runtime
        .create_presentation(&display, &backend)
        .expect("presentation");
    runtime
        .create_tearing_control_manager(&display, 1)
        .expect("tearing control");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = new_app(&runtime);
    app.by_id = true;
    app.client = Some(common::client::spawn_mapped_with_tearing(
        &socket,
        common::client::TearingSetup::Control,
    ));
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert!(
        app.committed >= 1,
        "the client commits at least once and commits reach surface_committed"
    );
    assert!(
        app.first_surface.is_some(),
        "the handler captured the live surface's id"
    );
    assert!(
        app.by_id_sampled_some >= 1,
        "sample_presentation(id) must reach the client's requested feedback"
    );
    assert!(
        app.by_id_hinted_vsync >= 1,
        "tearing_hint(id) must read the vsync default through the stored id"
    );
    assert!(
        app.by_id_control_some >= 1,
        "tearing_control(id) must resolve the client-created control through the stored id"
    );
}

/// The real send path: the handler builds a `PresentationEvent` from a real
/// present report against the live output and reports the first sampled
/// feedback with it. The client must observe `presented`, and the sent
/// event's fields must round-trip.
#[test]
fn reporting_a_sample_sends_presented_to_the_client() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    runtime
        .create_presentation(&display, &backend)
        .expect("presentation");
    runtime
        .create_tearing_control_manager(&display, 1)
        .expect("tearing control");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = new_app(&runtime);
    app.send_presented = true;
    app.client = Some(common::client::spawn_mapped(&socket));
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
        app.presented_sent, 1,
        "exactly the first sampled feedback is reported"
    );
    assert_eq!(
        app.presented_output, app.output,
        "the sent event names the output it was built against"
    );
    assert_eq!(
        app.presented_time,
        Some((9, 8)),
        "the sent event carries the reported timestamp"
    );
    assert_eq!(
        app.presented_refresh,
        Some(60_000),
        "the sent event carries the reported refresh"
    );
    assert_eq!(
        app.presented_seq,
        Some(7),
        "the sent event carries the reported sequence"
    );
    assert_eq!(
        app.presented_flags,
        Some(PresentFlags::VSYNC),
        "the sent event carries the reported flags"
    );
    assert!(
        events.feedback_presented,
        "the client observed `presented` for the reported sample"
    );
}

/// The async half of the hint contract: the client sets the async hint
/// before its first commit, and the server must observe `Some(Async)` plus
/// the current/pending/previous transitions across the two commits.
#[test]
fn an_async_hint_reaches_the_server_across_commits() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    runtime
        .create_tearing_control_manager(&display, 1)
        .expect("tearing control");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = new_app(&runtime);
    app.client = Some(common::client::spawn_mapped_with_tearing(
        &socket,
        common::client::TearingSetup::Async,
    ));
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert!(
        app.committed >= 2,
        "the mapped flow commits twice, so the hint transitions are observable"
    );
    assert!(
        app.hinted_async >= 1,
        "the server observed the async hint through tearing_hint"
    );
    assert!(
        app.hint_triples.len() >= 2,
        "a control object resolved on at least two commits"
    );
    assert!(
        app.hint_triples
            .iter()
            .all(|&(_, pending, _)| pending == TearingHint::Async),
        "the client asked for async once and never changed it, so pending stays async"
    );
    assert!(
        app.hint_triples
            .iter()
            .any(|&(current, _, _)| current == TearingHint::Async),
        "the committed hint became async"
    );
    assert!(
        app.hint_triples
            .iter()
            .any(|&(current, _, previous)| current == TearingHint::Async
                && previous == TearingHint::Vsync),
        "one commit moved the hint from vsync to async"
    );
    assert_eq!(
        app.hint_triples.last().map(|&(current, _, _)| current),
        Some(TearingHint::Async),
        "the hint stays async once applied"
    );
}
