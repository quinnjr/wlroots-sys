//! `Runtime::output_layout_box` and `Runtime::set_output_position` against a
//! real headless backend with two outputs.
//!
//! Two outputs rather than one because the interesting property —
//! non-overlapping auto placement — only exists once there is a second box
//! to be disjoint from. A single-output suite ([`headless.rs`](../headless.rs))
//! already covers the announce/frame/destroy lifecycle; this file is purely
//! about the layout, not re-proving that.

/// Note what neither handler below does: panic, or `unwrap`. A handler runs
/// underneath an `extern "C"` frame — see `OutputHandler`'s own docs — so
/// anything worth asserting is recorded here and checked once control is
/// back in the test.
fn headless_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "2");
    });
}

/// A second `init_output` for one output is an error, not a dead process.
///
/// `wlr_scene_output_create` plants an addon keyed by `(scene, output)`, and a
/// second call reaches `wlr_addon_init`'s `assert(0 && "Can't have two addons
/// of the same type with the same owner")` — which Arch compiles in, so the
/// process dies rather than returning anything. `add_scene_output` has probed
/// for this since it was written; `init_output` never did, and it is the call
/// every doc and example tells a compositor to make from `new_output`. A
/// re-plug racing a slow first init, or a consumer that also inits from its
/// own setup, took the whole compositor down.
///
/// The process surviving this test *is* the assertion.
#[test]
fn initialising_one_output_twice_is_refused_rather_than_fatal() {
    headless_env();
    struct App {
        runtime: wlr::Runtime,
        second: Option<bool>,
        turns: u32,
    }
    impl wlr::OutputHandler for App {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            let _ = output.enable_with_preferred_mode();
            let first = self.runtime.init_output(output);
            assert!(first.is_ok(), "the first init must succeed: {first:?}");
            // The call that used to abort.
            self.second = Some(self.runtime.init_output(output).is_err());
        }
    }
    impl wlr::ToplevelHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::FdHandler for App {}
    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            self.turns += 1;
            self.turns > 8 || self.second.is_some()
        }
    }
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    runtime.init_graphics(&display, &backend).expect("graphics");
    let mut app = App {
        runtime: runtime.clone(),
        second: None,
        turns: 0,
    };
    let _ = backend.run_all(&display, &mut app, &runtime, wlr::Until::Turns(12));
    assert_eq!(
        app.second,
        Some(true),
        "the second init must report an error, having reached new_output at all"
    );
}

/// `init_output` is what actually calls `wlr_output_layout_add_auto` (see
/// its own doc) — `enable_with_preferred_mode` alone only gives the output a
/// mode and a renderer target, not a place in the layout. Both tests below
/// call it for that reason, mirroring every other example/test in this crate
/// that brings an output up (`scene.rs`, `examples/scene_background.rs`, …).
#[test]
fn two_headless_outputs_get_disjoint_layout_boxes() {
    headless_env();
    struct App {
        boxes: Vec<(i32, i32, i32, i32)>,
        scheduled: usize,
        runtime: wlr::Runtime,
        turns: u32,
    }
    impl wlr::OutputHandler for App {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            let _ = output.enable_with_preferred_mode();
            let _ = self.runtime.init_output(output);
            // The id-keyed `schedule_frame` must resolve a live output id the
            // same way `output_layout_box` (called just below) does.
            if self.runtime.schedule_frame(output.id()).is_some() {
                self.scheduled += 1;
            }
            if let Some(b) = self.runtime.output_layout_box(output.id()) {
                self.boxes.push(b);
            }
        }
    }
    impl wlr::ToplevelHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::FdHandler for App {}
    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            self.turns += 1;
            self.turns > 8 || self.boxes.len() >= 2
        }
    }
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    runtime.init_graphics(&display, &backend).expect("graphics");
    let mut app = App {
        boxes: vec![],
        scheduled: 0,
        runtime: runtime.clone(),
        turns: 0,
    };
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(16))
        .expect("run");
    assert_eq!(app.boxes.len(), 2, "both outputs must be in the layout");
    assert_eq!(
        app.scheduled, 2,
        "schedule_frame must resolve every live output id it is handed"
    );
    let (a, b) = (app.boxes[0], app.boxes[1]);
    let disjoint = a.0 + a.2 <= b.0 || b.0 + b.2 <= a.0 || a.1 + a.3 <= b.1 || b.1 + b.3 <= a.1;
    assert!(
        disjoint,
        "auto layout must not overlap outputs: {a:?} vs {b:?}"
    );
}

/// Output tables are per-run — the same rule `runtime.rs`'s `clear_outputs`
/// documents for the identical toplevel table — so an id kept past the
/// `run_all` call that announced it must resolve to nothing, not to memory
/// wlroots may have already reused or freed for a later run's outputs.
#[test]
fn layout_box_after_the_run_is_stale_and_misses_cleanly() {
    headless_env();
    struct App {
        ids: Vec<wlr::OutputId>,
        runtime: wlr::Runtime,
        turns: u32,
    }
    impl wlr::OutputHandler for App {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            let _ = output.enable_with_preferred_mode();
            let _ = self.runtime.init_output(output);
            self.ids.push(output.id());
        }
    }
    impl wlr::ToplevelHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::FdHandler for App {}
    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            self.turns += 1;
            self.turns > 8 || !self.ids.is_empty()
        }
    }
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    runtime.init_graphics(&display, &backend).expect("graphics");
    let mut app = App {
        ids: vec![],
        runtime: runtime.clone(),
        turns: 0,
    };
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(16))
        .expect("run");
    let stale = *app.ids.first().expect("at least one output announced");
    // Output tables are per-run: after run_all returns the id is stale.
    assert_eq!(runtime.output_layout_box(stale), None);
    assert_eq!(runtime.set_output_position(stale, 0, 0), None);
    // `schedule_frame` shares the same id-resolution rule, so a stale id must
    // miss cleanly (return `None`) rather than touch freed/reused memory.
    assert_eq!(runtime.schedule_frame(stale), None);
}
