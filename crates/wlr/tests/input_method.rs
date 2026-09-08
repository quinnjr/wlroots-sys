//! A6.1 core relay: manager creation + double-create guard, driven headlessly.
//!
//! There is no public `test_support::headless_runtime()` helper — that shape
//! lives only in a private `#[cfg(test)] mod tests` unit module — so this
//! integration test builds the headless environment inline, mirroring the
//! per-binary setup that `tests/seat.rs` and `tests/decoration.rs` use.

/// The headless backend is selected by environment, read once when the
/// display and backend are created below.
fn headless_env() {
    // SAFETY: both tests in this binary set the same two variables to the same
    // values, and the test harness runs the tests in this binary serially by
    // default (no `#[test]` here spawns threads that read the environment), so
    // no other harness thread can observe a torn environment read even though
    // there is now more than one test.
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

#[test]
fn managers_register_listeners_without_a_client() {
    headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");

    rt.create_text_input_manager(&display)
        .expect("text-input manager create");
    rt.create_input_method_manager(&display)
        .expect("input-method manager create");

    // No client has bound anything, so the `new_text_input` / `new_input_method`
    // signals never fire. Driving one non-blocking dispatch iteration must not
    // fault, and the relay tables must stay empty — the resting state the
    // per-object listeners key their entries off of. The live second-IME-refused
    // behaviour is proven later in the icedtea harness (Part B, test 6) with real
    // clients, which a crate integration test cannot bind.
    display.event_loop().dispatch(0).expect("dispatch");
    assert_eq!(
        rt.rt_debug_text_input_count(),
        0,
        "no client bound, so the text-input table must be empty"
    );
}
