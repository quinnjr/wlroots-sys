//! xdg-decoration negotiation, against a real headless compositor with no
//! client.
//!
//! Same shape as `toplevels.rs`'s test file: what is provable without a
//! client library is that the decoration manager global can be created, that
//! the id-keyed mutator rejects an id that was never issued rather than
//! dereferencing it, and that the new handler method is additive.

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
fn decoration_manager_creates_once_on_a_display() {
    headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    // `create_xdg_shell` requires graphics to exist first (see its own doc);
    // `toplevels.rs`'s test carries the identical prerequisite.
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("shell");
    runtime
        .create_xdg_decoration_manager(&display)
        .expect("decoration manager");
}

#[test]
fn set_decoration_mode_on_a_dead_id_is_none() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    assert_eq!(
        runtime.set_decoration_mode(wlr::ToplevelId::dangling_for_test(), true),
        None
    );
}

#[test]
fn the_new_handler_method_is_additive() {
    struct Old;
    impl wlr::OutputHandler for Old {}
    impl wlr::ToplevelHandler for Old {}
    impl wlr::SeatHandler for Old {}
    impl wlr::FdHandler for Old {}
    impl wlr::LoopHandler for Old {}
    fn takes_handlers<S: wlr::Handlers>(_s: &S) {}
    takes_handlers(&Old);
}
