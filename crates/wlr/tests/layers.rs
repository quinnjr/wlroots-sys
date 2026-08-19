//! wlr-layer-shell, against a real headless compositor with no client.
//!
//! Same shape as `decoration.rs`'s test file: what is provable without a
//! client library is that the layer-shell global can be created, that the
//! id-keyed mutators reject an id that was never issued rather than
//! dereferencing it, and that the new handler methods are additive.
//!
//! The banded-tree scene tests (band stacking order, reparent-on-layer-
//! change, `raise_toplevel` staying within its band — see `Layer`'s own
//! doc) live in `src/runtime.rs`'s own `#[cfg(test)]` module instead of
//! here: they need to read a live `wlr_scene_tree`'s `children`/`parent`
//! fields directly, which are private to the crate, and an integration test
//! binary like this one only ever sees the crate's public surface — the
//! same reason this file cannot exercise anything client-driven either.

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`. See `toplevels.rs`'s
/// identical copy of this helper for the full argument — this file is a
/// separate integration-test binary with its own environment and its own
/// possible parallel `#[test]` threads, so it needs its own `Once`.
fn headless_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once::call_once` runs this closure at most once and blocks
        // every other caller of `call_once` on this `Once` until it returns,
        // so no concurrent `getenv` from another test's call to
        // `headless_env` can observe a torn write.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
        }
    });
}

#[test]
fn layer_shell_creates_once() {
    headless_env();
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_layer_shell(&display, 4)
        .expect("layer shell");
}

#[test]
fn layer_mutators_on_dead_ids_are_none() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let dead = wlr::LayerSurfaceId::dangling_for_test();
    assert_eq!(runtime.configure_layer_surface(dead, 10, 10), None);
    assert_eq!(runtime.set_layer_surface_position(dead, 0, 0), None);
    assert_eq!(runtime.focus_layer_keyboard(dead), None);
}

#[test]
fn add_rect_in_band_on_a_fresh_runtime_without_graphics_errors() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    // Mirrors add_rect's contract: no graphics yet -> Err, not panic.
    assert!(
        runtime
            .add_rect_in_band(wlr::Band::Overlay, 8, 8, [0.0, 0.0, 0.0, 1.0])
            .is_err()
    );
}

/// `OutputId` has no public dangling constructor (unlike `LayerSurfaceId`
/// and `ToplevelId`; see those types' own `dangling_for_test`), so a live
/// one is captured from a short `run_all`, the same way
/// `output_layout.rs`'s stale-id test does. What is under test here is the
/// *layer-surface* id miss specifically: `set_layer_surface_output`
/// resolves the layer id first (see that method's own doc), so a dead
/// layer id paired with a perfectly live, real output must still be
/// `None` — output resolution is never reached at all.
#[test]
fn set_layer_surface_output_on_dead_ids_is_none() {
    headless_env();
    struct App {
        output: Option<wlr::OutputId>,
        runtime: wlr::Runtime,
        turns: u32,
    }
    impl wlr::OutputHandler for App {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            let _ = output.enable_with_preferred_mode();
            let _ = self.runtime.init_output(output);
            self.output = Some(output.id());
        }
    }
    impl wlr::ToplevelHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::SessionLockHandler for App {}
    impl wlr::FdHandler for App {}
    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            self.turns += 1;
            self.turns > 8 || self.output.is_some()
        }
    }
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    runtime.init_graphics(&display, &backend).expect("graphics");
    let mut app = App {
        output: None,
        runtime: runtime.clone(),
        turns: 0,
    };
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(16))
        .expect("run");
    let output = app
        .output
        .expect("a headless output must have been announced");

    let dead_layer = wlr::LayerSurfaceId::dangling_for_test();
    assert_eq!(runtime.set_layer_surface_output(dead_layer, output), None);
}

#[test]
fn the_layer_methods_are_additive() {
    struct Old;
    impl wlr::OutputHandler for Old {}
    impl wlr::ToplevelHandler for Old {}
    impl wlr::SeatHandler for Old {}
    impl wlr::SessionLockHandler for Old {}
    impl wlr::FdHandler for Old {}
    impl wlr::LoopHandler for Old {}
    fn takes_handlers<S: wlr::Handlers>(_s: &S) {}
    takes_handlers(&Old);
}
