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
//! Handler observations are recorded on the app and asserted after the run,
//! never inside a handler, where a panic would abort through C.

use std::thread::JoinHandle;

use wlr::{Backend, Display, Output, OutputId, Runtime, Surface, SurfaceId, TearingHint, Until};

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
    /// Commits where `tearing_hint()` reported the vsync default.
    hinted_vsync: usize,
    /// Commits where `tearing_control()` correctly missed.
    control_missing: usize,
    /// Commits where the textured/scanned wrappers ran with a live output.
    textured: usize,
    scanned: usize,
    client: Option<JoinHandle<common::client::ClientEvents>>,
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &Output<'_>) {
        self.output = Some(output.id());
    }
}

impl wlr::ToplevelHandler for App {
    fn surface_committed(&mut self, surface: &Surface<'_>) {
        self.committed += 1;

        // Runs the sampled FFI against the real surface. With the client's
        // `wp_presentation.feedback` request applied, the first commit returns
        // `Some`; the wrapper detaches it and dropping it sends `discarded`.
        if surface.sampled().is_some() {
            self.sampled_some += 1;
        }

        // The tearing manager was cached onto this handle by `with_surface`; a
        // surface with no client-created control reports the wlroots default.
        if surface.tearing_hint() == Some(TearingHint::Vsync) {
            self.hinted_vsync += 1;
        }
        if surface.tearing_control().is_none() {
            self.control_missing += 1;
        }

        // Resolve the announced output to a live handle and run both
        // output-taking presentation wrappers against it.
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
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
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

    let mut app = App {
        runtime: runtime.clone(),
        output: None,
        committed: 0,
        sampled_some: 0,
        hinted_vsync: 0,
        control_missing: 0,
        textured: 0,
        scanned: 0,
        client: Some(common::client::spawn_mapped(&socket)),
    };

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
        app.hinted_vsync >= 1,
        "the tearing manager cached by with_surface must make tearing_hint report the default"
    );
    assert!(
        app.control_missing >= 1,
        "no client created a tearing control, so the object lookup must miss"
    );
}
