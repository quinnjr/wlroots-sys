//! Output protocol helpers, against a real headless backend.
//!
//! This integration binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local); the setup itself is `common::headless_env`,
//! shared with the other output test binaries. Handler observations are
//! recorded on `App` and asserted after the run — never inside a handler,
//! where a panic would abort through C.

mod common;

use common::headless_env;
use wlr::{Backend, BufferCaps, CommittedFields, Display, Output, Runtime, Transform, Until};

#[derive(Default)]
struct Observed {
    setup_commit_ok: Option<bool>,
    send_request_ok: Option<bool>,
    requested_all: Vec<CommittedFields>,
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
        req.set_transform(Transform::Flipped180);
        self.seen.send_request_ok = Some(output.send_request_state(&req).is_ok());
    }

    fn output_state_requested(&mut self, _output: &Output<'_>, fields: CommittedFields) {
        self.seen.requested_all.push(fields);
    }
}

impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {}

/// `send_request_state` accepts a staged transaction for its own output
/// and delivers the staged mask through `output_state_requested` — the
/// full emit-to-delivery round trip, with no protocol client in the loop.
#[test]
fn send_request_state_delivers_the_staged_mask() {
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
    // History, not last-write: the staged request must arrive intact among
    // whatever the backend emits around it.
    let staged = CommittedFields::SCALE | CommittedFields::TRANSFORM;
    assert!(
        !seen.requested_all.is_empty(),
        "at least one request must deliver"
    );
    assert!(
        seen.requested_all.contains(&staged),
        "the staged request must arrive intact; got {:?}",
        seen.requested_all
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
                // not per-call allocation luck. `None` agrees with `None`;
                // two `Some` sets agree when their owned contents match —
                // each entry copied out via `to_owned` (`DrmFormat` is
                // `PartialEq`; `DrmFormatSet` itself is not). This pins
                // determinism; the copy-out is pinned by the owned
                // `Result<Option<DrmFormatSet>>` signature — a borrow would
                // not survive past the call.
                self.stable = Some(match (first, second) {
                    (Ok(None), Ok(None)) => true,
                    (Ok(Some(a)), Ok(Some(b))) => {
                        let a: Vec<wlr::DrmFormat> = a.iter().map(|f| f.to_owned()).collect();
                        let b: Vec<wlr::DrmFormat> = b.iter().map(|f| f.to_owned()).collect();
                        a == b
                    }
                    _ => false,
                });
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
