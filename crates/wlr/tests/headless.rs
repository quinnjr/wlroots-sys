//! End-to-end: a headless backend announces an output, exactly once.
//!
//! This is the proof that the whole model works against real wlroots — handles,
//! ids, dispatch and delivery together. It needs no GPU and no seat.
//!
//! Frame and destroy delivery are proved by `backend.rs`'s own tests rather than
//! here: a headless output emits `frame` only once it has been enabled with a
//! mode, and this crate exposes none of the setters that would stage that —
//! `wlr_output_enable` and `wlr_output_set_mode` on this version — for
//! `Output::commit` to apply. Those tests drive the same `on_frame` /
//! `on_output_destroy` callbacks wlroots calls, against a real `wl_signal`.

use std::collections::HashMap;

/// Note what none of these handlers do: panic. A handler runs underneath an
/// `extern "C"` frame, so an `assert!` that fires takes the process down rather
/// than failing a test — see `OutputHandler`'s own docs. Anything worth
/// asserting is recorded here and checked once control is back in the test.
#[derive(Default)]
struct App {
    outputs: HashMap<wlr::OutputId, String>,

    /// Counted separately from `outputs.len()`, which cannot see a duplicate:
    /// an output announced twice carries the *same* [`wlr::OutputId`], so the
    /// second insert overwrites the first and the map still holds one entry.
    new_output_calls: u32,

    frames: u32,
    destroyed: Vec<wlr::OutputId>,
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        self.new_output_calls += 1;
        self.outputs
            .insert(output.id(), output.name().unwrap_or_default());
    }

    fn frame(&mut self, _output: &wlr::Output<'_>) {
        self.frames += 1;
    }

    fn destroyed(&mut self, id: wlr::OutputId) {
        self.outputs.remove(&id);
        self.destroyed.push(id);
    }
}

#[test]
fn headless_backend_announces_an_output_exactly_once() {
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

    backend.run(&mut app, 4).expect("run");

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
    // `frame` naming an output the handler was never told about is covered
    // where it can actually be exercised — `backend.rs`'s
    // `an_event_for_an_unknown_output_is_dropped_rather_than_delivered` — not
    // here: nothing in this test can make that happen (there is only ever one
    // output, and it is always announced before any frame could name it), so
    // an assertion for it here would be unfalsifiable rather than merely
    // redundant.
    assert_eq!(app.frames, 0, "nothing enabled the output, so no frames");
    assert!(app.destroyed.is_empty(), "nothing destroyed the output");

    let after_first_run = app.new_output_calls;
    assert_eq!(
        after_first_run, 1,
        "the one headless output must be announced once, not repeatedly"
    );

    // The second run is what pins `ensure_started`'s idempotence, and it is the
    // only thing that can. `wlr_backend_start` announces the outputs a backend
    // already has, so a `run` that started the backend again would re-announce
    // this one — and no assertion on `outputs` could tell: `ensure_id` is
    // idempotent, so the duplicate carries the same id and collapses back into
    // the same single map entry. Only the call count sees it.
    backend.run(&mut app, 2).expect("second run");

    assert_eq!(
        app.new_output_calls, after_first_run,
        "a second run must not re-start the backend and re-announce outputs \
         that were already announced"
    );
    // The larger consequence, per `Backend::run`'s own doc comment: the
    // `frame`/`destroy` listeners for an output announced during one `run`
    // live in a `Session` local to that call and unlink when it returns, and
    // `ensure_started` short-circuits on the second `run`, so nothing
    // re-registers them. The output announced above therefore gets *no*
    // further event at all from this second `run` — including `destroyed`.
    //
    // This asserts only the half that is reachable here: nothing arrived.
    // Proving the stronger claim — that the output really would have produced
    // a `frame` or `destroy` had the listeners still been live — is not
    // possible against a real headless backend with this crate's current
    // public surface: a headless output only emits `frame` once enabled with
    // a mode, and there is no exposed way to force that or to destroy an
    // output from safe `wlr` code. That half of the property is instead
    // covered directly, against a bare `wl_signal`, by `backend.rs`'s own
    // `a_frame_signal_reaches_the_frame_handler` and
    // `a_destroyed_output_is_forgotten_before_the_handler_is_told`.
    assert_eq!(
        app.frames, 0,
        "an output announced by an earlier run must not gain a frame \
         listener that a later run's dispatching could fire"
    );
    assert!(
        app.destroyed.is_empty(),
        "an output announced by an earlier run must not gain a destroy \
         listener either — its listeners unlinked with that run's Session, \
         and the second run's ensure_started short-circuit means nothing \
         re-registers them"
    );
}
