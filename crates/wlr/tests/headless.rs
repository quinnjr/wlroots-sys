//! End-to-end: a headless backend announces an output.
//!
//! This is the proof that the whole model works against real wlroots — handles,
//! ids, dispatch and delivery together. It needs no GPU and no seat.
//!
//! Frame and destroy delivery are proved by `backend.rs`'s own tests rather than
//! here: a headless output emits `frame` only once it has been enabled with a
//! mode, and this crate does not expose the `wlr_output_state` setters that
//! would take. Those tests drive the same `on_frame` / `on_output_destroy`
//! callbacks wlroots calls, against a real `wl_signal`.

use std::collections::HashMap;

#[derive(Default)]
struct App {
    outputs: HashMap<wlr::OutputId, String>,
    frames: u32,
    destroyed: Vec<wlr::OutputId>,
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        self.outputs
            .insert(output.id(), output.name().unwrap_or_default());
    }

    fn frame(&mut self, output: &wlr::Output<'_>) {
        assert!(
            self.outputs.contains_key(&output.id()),
            "frame for an output we were never told about"
        );
        self.frames += 1;
    }

    fn destroyed(&mut self, id: wlr::OutputId) {
        self.outputs.remove(&id);
        self.destroyed.push(id);
    }
}

#[test]
fn headless_backend_announces_an_output() {
    // wlroots reads both of these when the backend is created, so they have to
    // be in place before `autocreate`. Setting them here rather than relying on
    // the caller's environment keeps a plain `cargo test -p wlr` meaningful:
    // without `WLR_BACKENDS` wlroots would pick whatever the developer's
    // session offers (or nothing at all in CI), and the test would be measuring
    // the machine rather than this crate.
    //
    // SAFETY: `set_var` is unsound only against a concurrent reader of the
    // environment in another thread. This is the only test in this binary, so
    // the harness has started no other test thread, and nothing here has yet
    // called into wlroots or libc's locale/DNS machinery.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    }

    let display = wlr::Display::new().expect("display");
    // Declared after `display`, so it drops first: `Display::drop` destroys the
    // backend along with the event loop, and wlroots asserts that nothing is
    // still listening on `events.new_output` when it does.
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let mut app = App::default();

    backend.run(&display, &mut app, 4).expect("run");

    assert!(
        !app.outputs.is_empty(),
        "the headless backend should have announced at least one output"
    );
    assert!(
        app.outputs
            .values()
            .any(|name| name.starts_with("HEADLESS")),
        "the announced output should carry the backend's name, not an empty \
         string: {:?}",
        app.outputs
    );
    assert_eq!(app.frames, 0, "nothing enabled the output, so no frames");
    assert!(app.destroyed.is_empty(), "nothing destroyed the output");
}
