//! wlr-layer-shell, against a real headless compositor with no client.
//!
//! Same shape as `decoration.rs`'s test file: what is provable without a
//! client library is that the layer-shell global can be created, that the
//! id-keyed mutators reject an id that was never issued rather than
//! dereferencing it, and that the new handler methods are additive.

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
fn the_layer_methods_are_additive() {
    struct Old;
    impl wlr::OutputHandler for Old {}
    impl wlr::ToplevelHandler for Old {}
    impl wlr::SeatHandler for Old {}
    impl wlr::FdHandler for Old {}
    impl wlr::LoopHandler for Old {}
    fn takes_handlers<S: wlr::Handlers>(_s: &S) {}
    takes_handlers(&Old);
}
