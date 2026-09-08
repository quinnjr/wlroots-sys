//! A6.1 core relay: manager creation + double-create guard, driven headlessly.
//!
//! There is no public `test_support::headless_runtime()` helper — that shape
//! lives only in a private `#[cfg(test)] mod tests` unit module — so this
//! integration test builds the headless environment inline, mirroring the
//! per-binary setup that `tests/seat.rs` and `tests/decoration.rs` use.

/// The headless backend is selected by environment, read once when the
/// display and backend are created below.
fn headless_env() {
    // SAFETY: the only test in this binary, so no other harness thread can
    // observe a torn environment read.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    }
}

#[test]
fn create_text_input_and_input_method_managers_once() {
    headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");

    rt.create_text_input_manager(&display)
        .expect("first text-input create");
    rt.create_input_method_manager(&display)
        .expect("first input-method create");

    assert!(
        rt.create_text_input_manager(&display).is_err(),
        "double-create must error"
    );
    assert!(
        rt.create_input_method_manager(&display).is_err(),
        "double-create must error"
    );
}
