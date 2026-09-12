//! Output protocol helpers, against a real headless backend.
//!
//! Same shape as the other per-binary `headless_env` helpers: this
//! integration binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local). Handler observations are recorded on `App`
//! and asserted after the run — never inside a handler, where a panic
//! would abort through C.

use std::sync::Once;
use wlr::{Backend, BufferCaps, Display, Output, Runtime, Transform, Until};

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`. See `output.rs`'s
/// identical copy for the full argument — this is a separate integration-test
/// binary with its own environment.
fn headless_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once::call_once` runs this closure at most once and blocks
        // every other caller on this `Once` until it returns, so no concurrent
        // `getenv` can observe a torn write.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            std::env::set_var("WLR_RENDERER", "pixman");
        }
    });
}

#[derive(Default)]
struct Observed {
    setup_commit_ok: Option<bool>,
    send_request_ok: Option<bool>,
}

struct App {
    seen: Observed,
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &Output<'_>) {
        // Enable the output, then synthesize a scale request through
        // `send_request_state`: no protocol client is needed to exercise
        // the request_state delivery path. Recorded, never asserted: a
        // panic here would abort through C.
        let mut st = output.state();
        st.set_enabled(true);
        st.set_custom_mode(800, 600, 60_000);
        self.seen.setup_commit_ok = Some(st.commit().is_ok());

        let mut req = output.state();
        req.set_scale(2.0);
        req.set_transform(Transform::Normal);
        self.seen.send_request_ok = Some(output.send_request_state(&req).is_ok());
        // Delivery (`output_state_requested`) lands with the feedback
        // branch's request_state listener (PR #20): this tree emits into an
        // empty signal list, so the round trip asserts there, on rebase.
    }
}

impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {}

/// `send_request_state` accepts a staged transaction for its own output
/// and emits without trapping. The delivery half (`output_state_requested`
/// firing with the staged mask) asserts on rebase onto the feedback
/// branch, whose request_state listener receives the emission.
#[test]
fn send_request_state_call_is_sound() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    let mut app = App {
        seen: Observed::default(),
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");

    let seen = app.seen;
    assert_eq!(
        seen.setup_commit_ok,
        Some(true),
        "setup commit must succeed on headless"
    );
    assert_eq!(
        seen.send_request_ok,
        Some(true),
        "sending a staged request must succeed"
    );
}

/// The power manager global constructs once and refuses twice, same terms
/// as the presentation/tearing globals in `output_feedback.rs`.
#[test]
fn power_manager_global_constructs_once() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    runtime
        .create_power_manager(&display)
        .expect("power manager creates");
    assert!(
        matches!(
            runtime.create_power_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "second power manager create must refuse as a double-create"
    );

    // Running with the manager created executes the `set_mode` wiring block
    // (`if let Some(manager)` in run setup): a broken registration would
    // trap here, and an announced output proves the run still delivers.
    struct Announced {
        outputs: u32,
    }
    impl wlr::OutputHandler for Announced {
        fn new_output(&mut self, _output: &Output<'_>) {
            self.outputs += 1;
        }
    }
    impl wlr::ToplevelHandler for Announced {}
    impl wlr::SeatHandler for Announced {}
    impl wlr::FdHandler for Announced {}
    impl wlr::LoopHandler for Announced {}

    let mut app = Announced { outputs: 0 };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    assert_eq!(
        app.outputs, 1,
        "run with power manager must announce the output"
    );
}

/// `primary_formats` is smoke-only: headless may constrain formats or not,
/// and either answer is backend truth, not something to pin. What this
/// proves is the call contract: no trap through C, a stable answer across
/// calls, and `None` (unconstrained) distinguished from `Some` — never an
/// error in disguise.
#[test]
fn primary_formats_call_is_sound() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    struct Probe {
        stable: Option<bool>,
    }
    impl wlr::OutputHandler for Probe {
        fn new_output(&mut self, output: &Output<'_>) {
            let mut st = output.state();
            st.set_enabled(true);
            st.set_custom_mode(800, 600, 60_000);
            if st.commit().is_ok() {
                let first = output.primary_formats(BufferCaps::DMABUF);
                let second = output.primary_formats(BufferCaps::DMABUF);
                // Both calls must agree: the constraint is backend state,
                // not per-call allocation luck. `is_ok` on both pins the
                // copy-out too — an allocation failure would surface here.
                self.stable = Some(
                    first.is_ok()
                        && second.is_ok()
                        && first.unwrap().is_none() == second.unwrap().is_none(),
                );
            }
        }
    }
    impl wlr::ToplevelHandler for Probe {}
    impl wlr::SeatHandler for Probe {}
    impl wlr::FdHandler for Probe {}
    impl wlr::LoopHandler for Probe {}

    let mut app = Probe { stable: None };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    assert_eq!(
        app.stable,
        Some(true),
        "primary_formats must agree with itself across calls"
    );
}
