//! A6.1 core relay: manager creation + double-create guard, driven headlessly.
//!
//! There is no public `test_support::headless_runtime()` helper — that shape
//! lives only in a private `#[cfg(test)] mod tests` unit module — so this
//! integration test builds the headless environment inline, mirroring the
//! per-binary setup that `tests/seat.rs` and `tests/decoration.rs` use.

/// The headless backend is selected by environment, read once when the
/// display and backend are created below.
fn headless_env() {
    // SAFETY: libtest runs the tests in a binary in parallel by default, so
    // this is *not* safe by virtue of serial execution. It is safe because all
    // three tests in this binary set the same two variables to the same values
    // (and no `#[test]` here spawns threads that read the environment): a
    // concurrent write from another test writes byte-identical values, so no
    // harness thread can observe a torn environment read.
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
    // The A6.2 popup table shares the same resting state: no input-method has
    // bound, so `new_popup_surface` never fired and the table stays empty. The
    // live popup-created/keyboard-grab behaviour is proven in the icedtea harness
    // (Part B, tests 8-9) with real clients, which a crate test cannot bind.
    assert_eq!(
        rt.rt_debug_input_popup_count(),
        0,
        "no input-method bound, so the popup table must be empty"
    );
}

#[test]
fn relay_focus_is_a_noop_with_no_text_inputs() {
    headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");
    rt.create_seat(&display, "seat0").expect("seat");
    rt.create_text_input_manager(&display)
        .expect("text-input manager create");
    rt.create_input_method_manager(&display)
        .expect("input-method manager create");

    // The keyboard-focus relay must survive being driven with empty relay
    // tables: no text-inputs, no input-method, nothing focused. `clear` walks
    // the outgoing path (from a null outgoing surface) and must not panic. The
    // non-vacuous enter/leave behaviour is proven in the icedtea harness with
    // two real clients (Part B, tests 2-3), which a crate test cannot bind.
    rt.clear_keyboard_focus();
    assert_eq!(
        rt.rt_debug_text_input_count(),
        0,
        "relay must not have mutated the empty text-input table"
    );
}

#[test]
fn dangling_ids_and_no_ime_read_empty_snapshots() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let bogus = wlr::InputPopupSurfaceId::dangling_nth_for_test(0);
    assert!(runtime.pending_ime_state().is_none());
    assert!(runtime.committed_ime_state().is_none());
    assert!(runtime.pending_text_input_state().is_none());
    assert!(runtime.committed_text_input_state().is_none());
    // SAFETY: null is the one argument the downcast never dereferences — the
    // implementation null-checks first and reports the miss as `None`.
    assert!(unsafe { runtime.try_input_popup_surface(std::ptr::null_mut()) }.is_none());
    assert!(!runtime.destroy_keyboard_grab());
    assert!(runtime.input_popup_surface(bogus).is_none());
}

/// A `SeatHandler` written against the release before this one, with an empty
/// body, must still compile and still be usable now: the IME-commit
/// notification method is defaulted, so it does not appear in a legacy impl.
/// That is the additivity claim of this task, and it is a compile-time claim,
/// so the test that asserts it is a type that exists.
struct LegacyCommitHandler;

impl wlr::SeatHandler for LegacyCommitHandler {}

/// A handler that overrides the commit-notification method, proving the
/// signature is what the contract froze: no payload — the handler reads the
/// committed generation back via `Runtime::committed_ime_state()`.
#[derive(Default)]
struct CommitHandler {
    seen: u32,
}

impl wlr::SeatHandler for CommitHandler {
    fn input_method_committed(&mut self) {
        self.seen += 1;
    }
}

#[test]
fn the_ime_commit_notification_hook_is_additive_and_overridable() {
    let _legacy = LegacyCommitHandler;
    let handler = CommitHandler::default();
    assert_eq!(handler.seen, 0);
}
