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
        runtime: wlr::Runtime,
        turns: u32,
    }
    impl wlr::OutputHandler for App {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            let _ = output.enable_with_preferred_mode();
            let _ = self.runtime.init_output(output);
            if let Some(b) = self.runtime.output_layout_box(output.id()) {
                self.boxes.push(b);
            }
        }
    }
    impl wlr::ToplevelHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::SessionLockHandler for App {}
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
        runtime: runtime.clone(),
        turns: 0,
    };
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(16))
        .expect("run");
    assert_eq!(app.boxes.len(), 2, "both outputs must be in the layout");
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
    impl wlr::SessionLockHandler for App {}
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
}
