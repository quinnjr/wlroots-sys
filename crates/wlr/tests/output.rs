//! Output atomic state and accessors, against a real headless backend.
//!
//! Same shape as the other per-binary `headless_env` helpers: this
//! integration binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local). Handler observations are recorded on `App`
//! and asserted after the run — never inside a handler, where a panic
//! would abort through C.

use std::sync::Once;
use wlr::{Backend, Display, OutputState, Runtime, Until};

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`. See `axis.rs`'s
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
struct App {
    staged_fields: Option<u32>,
    mode_type: Option<Option<wlr::ModeType>>,
    commit_ok: Option<bool>,
    headless: Option<bool>,
    name_round_trip: Option<bool>,
    effective: Option<(i32, i32)>,
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        // Stage several fields and check the staged mask before committing:
        // one atomic transaction instead of one commit per field.
        let mut st = output.state();
        st.set_enabled(true);
        st.set_scale(2.0);
        st.set_adaptive_sync_enabled(false);
        self.staged_fields = Some(st.committed_fields());
        self.mode_type = Some(st.mode_type());
        self.commit_ok = Some(st.commit().is_ok());

        // A dropped transaction finishes without committing: must not abort
        // or corrupt the next commit.
        {
            let mut abandoned = output.state();
            abandoned.set_scale(3.0);
        }

        self.headless = Some(output.is_headless());
        self.name_round_trip = Some(
            output.set_name("m6-output-test").is_ok()
                && output.name().as_deref() == Some("m6-output-test"),
        );
        self.effective = Some(output.effective_resolution());
    }
}

impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        true
    }
}

/// Atomic state stages fields and commits them together on a headless
/// output; the accessors read back live truth.
#[test]
fn output_state_stages_fields_and_commits_atomically() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    let mut app = App::default();
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");

    assert_eq!(
        app.staged_fields,
        Some(
            OutputState::FIELD_ENABLED
                | OutputState::FIELD_SCALE
                | OutputState::FIELD_ADAPTIVE_SYNC_ENABLED
        ),
        "the staged mask must name exactly the fields set"
    );
    assert_eq!(
        app.commit_ok,
        Some(true),
        "an enabled+scale transaction must commit on headless"
    );
    assert_eq!(app.headless, Some(true), "headless backend headless");
    assert_eq!(
        app.name_round_trip,
        Some(true),
        "set_name must read back through name()"
    );
    let (w, h) = app.effective.expect("effective resolution read");
    assert!(w > 0 && h > 0, "effective resolution must be positive");
}

/// `OutputState::copy_from` carries staged fields across transactions.
#[test]
fn output_state_copy_carries_staged_fields() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    struct CopyApp {
        copied_fields: Option<u32>,
    }
    impl wlr::OutputHandler for CopyApp {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            let mut src = output.state();
            src.set_scale(2.0);
            let mut dst = output.state();
            dst.copy_from(&src);
            self.copied_fields = Some(dst.committed_fields());
        }
    }
    impl wlr::ToplevelHandler for CopyApp {}
    impl wlr::SeatHandler for CopyApp {}
    impl wlr::FdHandler for CopyApp {}
    impl wlr::LoopHandler for CopyApp {
        fn should_stop(&mut self) -> bool {
            true
        }
    }

    let mut app = CopyApp {
        copied_fields: None,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    assert_eq!(
        app.copied_fields,
        Some(OutputState::FIELD_SCALE),
        "copy_from must carry the staged scale bit"
    );
}
