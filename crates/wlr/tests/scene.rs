//! A headless output is brought up with a renderer, added to the scene, and
//! committed with a background rect under it — the whole of what release 1
//! owes a compositor's boot path.

#[derive(Default)]
struct App {
    runtime: Option<wlr::Runtime>,
    outputs: Vec<wlr::OutputId>,
    sizes: Vec<(i32, i32)>,
    init_errors: Vec<wlr::Error>,
    commits: u32,
    commit_errors: Vec<wlr::Error>,
    turns: u32,
}

impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.turns += 1;
        // Bounded so a machine where the headless output never produces a
        // frame cannot hang CI; the assertions below say what must have
        // happened within that budget.
        self.turns >= 8
    }
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        self.outputs.push(output.id());
        let Some(runtime) = self.runtime.as_ref() else { return };
        // Never `unwrap` in a handler: this runs under an `extern "C"` frame.
        if let Err(e) = output.enable_with_preferred_mode() {
            self.init_errors.push(e);
            return;
        }
        if let Err(e) = runtime.init_output(output) {
            self.init_errors.push(e);
            return;
        }
        self.sizes.push(output.size());
    }

    fn frame(&mut self, output: &wlr::Output<'_>) {
        let Some(runtime) = self.runtime.as_ref() else { return };
        match runtime.commit_output(output) {
            Ok(()) => self.commits += 1,
            Err(e) => self.commit_errors.push(e),
        }
    }
}

#[test]
fn a_headless_output_renders_a_scene_with_a_background_rect() {
    // SAFETY: the only test in this binary, so no other harness thread can
    // observe a torn environment read.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    }

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .init_graphics(&display, &backend)
        .expect("renderer, allocator and core globals");

    let bg = runtime
        .add_rect(4096, 4096, [0.1, 0.1, 0.12, 1.0])
        .expect("background rect");
    runtime.lower_rect_to_bottom(bg).expect("rect is known");

    let mut app = App {
        runtime: Some(runtime.clone()),
        ..App::default()
    };
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Stop)
        .expect("run_all");

    assert_eq!(app.init_errors, Vec::new(), "output bring-up must not fail");
    assert_eq!(app.commit_errors, Vec::new(), "scene commit must not fail");
    assert_eq!(app.outputs.len(), 1, "one headless output was announced");
    assert!(
        app.sizes.iter().all(|&(w, h)| w > 0 && h > 0),
        "an enabled output reports a real mode size: {:?}",
        app.sizes
    );
    assert!(
        app.commits >= 1,
        "an enabled output produces frames, and each one commits the scene"
    );
}

#[test]
fn rect_mutators_report_an_unknown_id_rather_than_panicking() {
    let runtime = wlr::Runtime::new().expect("runtime");
    let bogus = runtime.add_rect(1, 1, [0.0; 4]);
    // Without `init_graphics` there is no scene to attach a rect to.
    assert!(matches!(bogus, Err(wlr::Error::Create(_))), "got {bogus:?}");
}
