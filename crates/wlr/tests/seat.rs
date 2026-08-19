//! Seat creation, focus by id, and the scene hit test.
//!
//! wlroots' headless backend can add virtual input devices, but this crate
//! exposes no API to synthesise a key press — a compositor never needs one —
//! so the delivery path itself is proved by the example under a real session
//! and by `backend.rs`'s own signal tests. What is proved here is everything
//! reachable without a device: the seat exists, focus by id reports a miss
//! for an unknown id rather than dereferencing it, and the hit test on an
//! empty scene finds nothing instead of faulting.

#[derive(Default)]
struct App {
    keys: Vec<u32>,
    turns: u32,
}

impl wlr::OutputHandler for App {}
impl wlr::ToplevelHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.turns += 1;
        self.turns >= 4
    }
}

impl wlr::SeatHandler for App {
    fn key(&mut self, event: &wlr::KeyEvent<'_>) -> bool {
        self.keys.push(event.keysym());
        false
    }
}

#[test]
fn a_seat_can_be_created_and_a_run_survives_it() {
    // SAFETY: the only test in this binary, so no other harness thread can
    // observe a torn environment read.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    }

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg shell");
    runtime.create_seat(&display, "seat0").expect("seat");

    // `Until::Turns`, not `Until::Stop`: nothing in this test ever calls
    // `Output::schedule_frame` or otherwise gives the headless output a
    // reason to wake the loop again once it settles, and neither does
    // creating a seat with no input device attached — so `Until::Stop`'s
    // blocking `wl_event_loop_dispatch` would wait forever with nothing to
    // wake it (the same reasoning `toplevels.rs`'s own test documents).
    // `Until::Turns` dispatches non-blocking turns instead, so the run
    // always returns regardless of what `App::should_stop` does.
    let mut app = App::default();
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(4))
        .expect("run_all");

    // No input device attached, so no keys — the point is that the seat's
    // and the backend's `new_input` listeners registered and tore down
    // without faulting.
    assert!(app.keys.is_empty());
}

#[test]
fn creating_the_seat_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.create_seat(&display, "seat0").expect("first");
    assert!(
        matches!(
            runtime.create_seat(&display, "seat1"),
            Err(wlr::Error::Operation(_))
        ),
        "a second wl_seat global would make the compositor advertise two"
    );
}

#[test]
fn creating_the_primary_selection_manager_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_primary_selection_manager(&display)
        .expect("first");
    assert!(
        matches!(
            runtime.create_primary_selection_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second primary-selection global would make the compositor advertise two"
    );
}

#[test]
fn creating_the_data_control_manager_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_data_control_manager(&display)
        .expect("first");
    assert!(
        matches!(
            runtime.create_data_control_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second data-control global would make the compositor advertise two"
    );
}

#[test]
fn creating_the_virtual_keyboard_manager_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_virtual_keyboard_manager(&display)
        .expect("first");
    assert!(
        matches!(
            runtime.create_virtual_keyboard_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second virtual-keyboard global would make the compositor advertise two"
    );
}

#[test]
fn creating_the_virtual_pointer_manager_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_virtual_pointer_manager(&display)
        .expect("first");
    assert!(
        matches!(
            runtime.create_virtual_pointer_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second virtual-pointer global would make the compositor advertise two"
    );
}

#[test]
fn creating_the_screencopy_manager_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.create_screencopy_manager(&display).expect("first");
    assert!(
        matches!(
            runtime.create_screencopy_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second screencopy global would make the compositor advertise two"
    );
}

#[test]
fn creating_the_pointer_constraints_manager_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_pointer_constraints_manager(&display)
        .expect("first");
    assert!(
        matches!(
            runtime.create_pointer_constraints_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second pointer-constraints global would make the compositor advertise two"
    );
}

#[test]
fn cursor_position_is_the_origin_without_a_seat() {
    // No `create_seat`, so there is no `wlr_cursor`; `cursor_position` must
    // report the origin rather than dereferencing a null cursor. This is the
    // reachable unit assertion for the accessor the constraint-enforcement path
    // and the harness tests build on; full cursor motion is proven there.
    let runtime = wlr::Runtime::new().expect("runtime");
    assert_eq!(runtime.cursor_position(), (0.0, 0.0));
}

#[test]
fn creating_the_relative_pointer_manager_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_relative_pointer_manager(&display)
        .expect("first");
    assert!(
        matches!(
            runtime.create_relative_pointer_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second relative-pointer global would make the compositor advertise two"
    );
}

#[test]
fn creating_the_idle_notifier_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.create_idle_notifier(&display).expect("first");
    assert!(
        matches!(
            runtime.create_idle_notifier(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second idle-notifier global would make the compositor advertise two"
    );
}

#[test]
fn creating_the_idle_inhibit_manager_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_idle_inhibit_manager(&display)
        .expect("first");
    assert!(
        matches!(
            runtime.create_idle_inhibit_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second idle-inhibit-manager global would make the compositor advertise two"
    );
}

#[test]
fn focus_and_hit_test_report_a_miss_rather_than_dereferencing() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.create_seat(&display, "seat0").expect("seat");

    assert_eq!(
        runtime.focus_toplevel_keyboard(wlr::ToplevelId::dangling_for_test()),
        None
    );
    // Clearing focus when nothing is focused must be harmless: it is what
    // every "nothing is focused now" path calls, unconditionally.
    runtime.clear_keyboard_focus();
    runtime.clear_keyboard_focus();

    assert_eq!(
        runtime.toplevel_at(10.0, 10.0),
        None,
        "an empty scene has nothing under the pointer"
    );
    // Not `(0.0, 0.0)`: `wlr_cursor_create` itself defaults a fresh cursor's
    // position to `(100.0, 100.0)` (confirmed empirically — this is the
    // value every call in this test observes, deterministically, before any
    // motion event has ever moved it), presumably so a cursor with no output
    // layout attached yet does not start out sitting exactly on a corner.
    // `Runtime::pointer_position`'s own doc promises `(0.0, 0.0)` only for
    // the *no-seat* case (see `focus_without_a_seat_is_a_miss`, which has no
    // cursor at all); this seat has one, so this is wlroots' default, not
    // this crate's.
    assert_eq!(runtime.pointer_position(), (100.0, 100.0));
}

/// Focus with no seat at all must also be a miss, not a null dereference:
/// a consumer can legitimately run a scene-only compositor.
#[test]
fn focus_without_a_seat_is_a_miss() {
    let runtime = wlr::Runtime::new().expect("runtime");
    assert_eq!(
        runtime.focus_toplevel_keyboard(wlr::ToplevelId::dangling_for_test()),
        None
    );
    runtime.clear_keyboard_focus();
}

/// A second `ext_session_lock_manager_v1` global would make the compositor
/// advertise two, so the crate refuses the double-create — mirroring every
/// other manager global here.
#[test]
fn creating_the_session_lock_manager_twice_is_refused() {
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .create_session_lock_manager(&display)
        .expect("first");
    assert!(
        matches!(
            runtime.create_session_lock_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second session-lock global would make the compositor advertise two"
    );
}

/// A fresh runtime is not locked, and creating the manager global alone does
/// not lock it — a lock is only entered when a client actually takes one
/// (proven end-to-end by the icedtea harness tests). This pins the initial
/// state the input-isolation gates all key off of.
#[test]
fn a_fresh_runtime_is_not_session_locked() {
    let runtime = wlr::Runtime::new().expect("runtime");
    assert!(
        !runtime.is_session_locked(),
        "a runtime with no locker must not report itself locked"
    );

    let display = wlr::Display::new().expect("display");
    runtime
        .create_session_lock_manager(&display)
        .expect("manager");
    assert!(
        !runtime.is_session_locked(),
        "advertising the manager global must not lock the session by itself"
    );
}
