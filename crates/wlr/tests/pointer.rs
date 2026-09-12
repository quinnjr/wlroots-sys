//! M7 cursor depth: snapshot readers miss cleanly with no seat.

use wlr::CursorId;

/// The headless backend is selected by environment, read once when the
/// display and backend are created below.
fn headless_env() {
    // SAFETY: libtest runs the tests in a binary in parallel by default, so
    // this is *not* safe by virtue of serial execution. It is safe because all
    // tests in this binary set the same two variables to the same values (and
    // no `#[test]` here spawns threads that read the environment): a
    // concurrent write from another test writes byte-identical values, so no
    // harness thread can observe a torn environment read.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    }
}

#[test]
fn dangling_cursor_misses_cleanly() {
    headless_env();
    let rt = wlr::Runtime::new().unwrap();
    assert!(rt.cursor_state().is_none());
    assert!(rt.try_cursor(CursorId::dangling()).is_none());
}

#[test]
fn cursor_map_misses_without_seat() {
    headless_env();
    let rt = wlr::Runtime::new().unwrap();
    assert!(
        rt.map_cursor_to_region(wlr::Box2D::new(0, 0, 100, 100))
            .is_none(),
        "no seat cursor, so no mapping applies"
    );
}

#[test]
fn cursor_appears_with_seat() {
    use wlr::CursorImage;

    headless_env();
    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");

    assert!(rt.cursor_id().is_none(), "no seat yet, so no cursor id");

    rt.create_seat(&display, "seat0").expect("seat");

    let id = rt.cursor_id().expect("seat brings a cursor");
    let state = rt.cursor_state().expect("tracked cursor snapshots");
    assert_eq!(
        state,
        rt.try_cursor(id).expect("id resolves to the same state")
    );
    // The position is whatever wlroots initialised the cursor to — the
    // snapshot reports the live value, which is what `pointer_position`
    // reads from the same cursor, not a constant this test invents.
    assert_eq!(state.position, rt.pointer_position());
    assert_eq!(state.mapped_output, None);
    assert_eq!(state.mapped_region, None);
    assert_eq!(state.image, CursorImage::Hidden);
    assert_eq!(state.hotspot, (0, 0));
    assert!(rt.try_cursor(CursorId::dangling()).is_none());
}

#[test]
fn xcursor_theme_destroy_roundtrip() {
    use wlr::XcursorManagerId;

    headless_env();
    let rt = wlr::Runtime::new().unwrap();

    // Unknown ids are harmless no-ops, never double-frees.
    assert!(!rt.destroy_xcursor_theme(XcursorManagerId::dangling()));

    // Whether this container has themes installed decides `Some` vs `None`
    // here, so both outcomes are accepted — but a loaded theme must round
    // trip through destroy exactly once.
    if let Some(id) = rt.load_xcursor_theme("default", 24) {
        assert!(rt.destroy_xcursor_theme(id), "first destroy releases");
        assert!(!rt.destroy_xcursor_theme(id), "second destroy misses");
    }
    // A NUL in the name can never load.
    assert!(rt.load_xcursor_theme("def\0ault", 24).is_none());
}

/// M7 Task 2 compile assertion: the pointer-protocol handler surface.
///
/// Overrides the id-only event hooks, so this binary fails to build (E0407:
/// no such method on `SeatHandler`) until the defaulted
/// `pointer_constraint_committed` / `gesture_began` methods land. The struct
/// is constructed in `protocol_hooks_are_overridable` below, so every field
/// is read and nothing here is dead code once the methods exist.
struct ProtocolHandler {
    constraints: Vec<wlr::ConstraintId>,
    gestures_began: Vec<wlr::GestureId>,
}

impl wlr::SeatHandler for ProtocolHandler {
    fn pointer_constraint_committed(&mut self, _id: wlr::ConstraintId) {
        self.constraints.push(_id);
    }

    fn gesture_began(&mut self, _id: wlr::GestureId) {
        self.gestures_began.push(_id);
    }
}

#[test]
fn protocol_hooks_are_overridable() {
    let handler = ProtocolHandler {
        constraints: Vec::new(),
        gestures_began: Vec::new(),
    };
    assert!(handler.constraints.is_empty());
    assert!(handler.gestures_began.is_empty());
}

/// A `SeatHandler` written before the pointer-protocol hooks existed, with an
/// empty body, must still compile — the A12 additivity claim of this task.
struct LegacyProtocolHandler;

impl wlr::SeatHandler for LegacyProtocolHandler {}

#[test]
fn protocol_hooks_are_additive() {
    let _legacy = LegacyProtocolHandler;
}

#[test]
fn gestures_manager_double_create_is_refused() {
    headless_env();
    let display = wlr::Display::new().expect("display");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.create_pointer_gestures_manager(&display).expect("first");
    assert!(
        matches!(
            rt.create_pointer_gestures_manager(&display),
            Err(wlr::Error::Operation(_))
        ),
        "a second zwp_pointer_gestures_v1 global would double-advertise the protocol"
    );
}

/// M7 Task 3: touch + switch miss shapes across the seat lifecycle.
///
/// `touch_state` is `None` with no seat and with a seat that has no touch
/// capability, `Some` once touch is enabled — the headless stand-in for a
/// touch-device attach, since no virtual-touch protocol exists to deliver a
/// real one — and `None` again once the seat is destroyed. `switch_state`
/// has no headless driver (no switch hardware here), so it only proves the
/// miss shape.
#[test]
fn touch_miss_is_clean() {
    headless_env();
    let rt = wlr::Runtime::new().unwrap();
    assert!(rt.touch_state().is_none(), "no seat, so no touch state");
    assert!(rt.switch_state().is_none(), "no seat, so no switch state");

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    rt.init_graphics(&display, &backend).expect("graphics");
    rt.create_seat(&display, "seat0").expect("seat");

    // A seat with no touch capability and no switch device reports neither.
    assert!(rt.touch_state().is_none());
    assert!(rt.switch_state().is_none());

    // Enabling touch (the test-harness stand-in for attaching a touch
    // device) flips the snapshot on: no points down, no grab held.
    rt.enable_test_touch();
    let state = rt.touch_state().expect("touch-enabled seat snapshots");
    assert!(state.points.is_empty());
    assert!(!state.has_grab);

    // Destroying the seat drops both snapshots again; a second destroy is a
    // harmless miss rather than a double-free.
    assert!(rt.destroy_seat(), "a live seat destroys");
    assert!(rt.touch_state().is_none());
    assert!(rt.switch_state().is_none());
    assert!(!rt.destroy_seat(), "no seat left to destroy");
}

/// M7 Task 3 compile assertion: the touch/switch handler surface.
///
/// Overrides the id-only event hooks, so this binary fails to build (E0407:
/// no such method on `SeatHandler`) until the defaulted `touch_down` /
/// `touch_up` / `touch_cancelled` / `switch_toggled` methods land. Same shape
/// as Task 2's `ProtocolHandler` above.
struct TouchSwitchHandler {
    downs: Vec<wlr::TouchId>,
    ups: Vec<wlr::TouchId>,
    cancelled: u32,
    toggles: Vec<(wlr::SwitchId, bool)>,
}

impl wlr::SeatHandler for TouchSwitchHandler {
    fn touch_down(&mut self, _id: wlr::TouchId) {
        self.downs.push(_id);
    }

    fn touch_up(&mut self, _id: wlr::TouchId) {
        self.ups.push(_id);
    }

    fn touch_cancelled(&mut self) {
        self.cancelled += 1;
    }

    fn switch_toggled(&mut self, _id: wlr::SwitchId, _on: bool) {
        self.toggles.push((_id, _on));
    }
}

#[test]
fn touch_switch_hooks_are_overridable() {
    let handler = TouchSwitchHandler {
        downs: Vec::new(),
        ups: Vec::new(),
        cancelled: 0,
        toggles: Vec::new(),
    };
    assert!(handler.downs.is_empty());
    assert!(handler.ups.is_empty());
    assert_eq!(handler.cancelled, 0);
    assert!(handler.toggles.is_empty());
}

/// A `SeatHandler` written before the touch/switch hooks existed, with an
/// empty body, must still compile — the A12 additivity claim of this task.
struct LegacyTouchSwitchHandler;

impl wlr::SeatHandler for LegacyTouchSwitchHandler {}

#[test]
fn touch_switch_hooks_are_additive() {
    let _legacy = LegacyTouchSwitchHandler;
}

/// Touch and switch ids are plain values from outside the crate — which is
/// what lets a consumer match down/up pairs on them and log toggles.
#[test]
fn touch_switch_ids_are_usable_from_outside_the_crate() {
    let a = wlr::TouchId::dangling_nth_for_test(1);
    let b = wlr::TouchId::dangling_nth_for_test(2);
    assert_ne!(a, b);
    assert_eq!(a, wlr::TouchId::dangling_nth_for_test(1));

    let c = wlr::SwitchId::dangling_nth_for_test(1);
    let d = wlr::SwitchId::dangling_nth_for_test(2);
    assert_ne!(c, d);
    assert_eq!(format!("{c:?}"), "SwitchId(..)");
}
