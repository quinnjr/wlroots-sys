//! M7 cursor depth: snapshot readers miss cleanly with no seat.

mod common;

use wlr::CursorId;

#[test]
fn dangling_cursor_misses_cleanly() {
    common::headless_env();
    let rt = wlr::Runtime::new().unwrap();
    assert!(rt.cursor_state().is_none());
    assert!(rt.try_cursor(CursorId::dangling()).is_none());
}

#[test]
fn cursor_map_misses_without_seat() {
    common::headless_env();
    let rt = wlr::Runtime::new().unwrap();
    assert!(
        rt.map_cursor_to_region(wlr::Box2D::new(0, 0, 100, 100))
            .is_none(),
        "no seat cursor, so no mapping applies"
    );
}

#[test]
fn cursor_appears_with_seat() {
    let _serial = common::headless_guard();
    use wlr::CursorImage;

    common::headless_env();
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

    common::headless_env();
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
}

/// A NUL in the xcursor theme name can never load, in either build mode.
///
/// The refusal is a programming error — `load_xcursor_theme`'s
/// `debug_assert!` fires in debug builds before the `None` is reached — so
/// the attempt is caught: debug observes the gate's message, release
/// observes the quiet `None`. The same shape as
/// `destroy_seat_refused_inside_handler` below, and the integration twin of
/// the `xcursor_theme_nul_name_is_refused_in_both_modes` unit test.
#[test]
fn xcursor_theme_nul_name_is_refused_in_both_modes() {
    common::headless_env();
    let rt = wlr::Runtime::new().unwrap();

    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.load_xcursor_theme("def\0ault", 24)
    }));
    if cfg!(debug_assertions) {
        let message = attempt.expect_err("the debug gate must fire, not load");
        let message = message
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| message.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| "(non-string panic)".to_string());
        assert!(
            message.contains("NUL"),
            "the gate names the fault: {message}"
        );
    } else {
        assert!(
            attempt.expect("no panic in release").is_none(),
            "a NUL name never loads"
        );
    }
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
    let _serial = common::headless_guard();
    common::headless_env();
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
    let _serial = common::headless_guard();
    common::headless_env();
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

/// M7 review (a): `map_cursor_to_output` misses without a seat and for an
/// unknown output, and records a live mapping in the snapshot.
#[test]
fn cursor_output_mapping_misses_and_records() {
    let _serial = common::headless_guard();
    common::headless_env();

    let rt = wlr::Runtime::new().unwrap();
    assert!(
        rt.map_cursor_to_output(wlr::OutputId::dangling_for_test())
            .is_none(),
        "no seat cursor, so no mapping applies"
    );

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");
    rt.create_seat(&display, "seat0").expect("seat");

    // A seat with no announced output yet: a dangling id still misses, and
    // nothing is recorded.
    assert!(
        rt.map_cursor_to_output(wlr::OutputId::dangling_for_test())
            .is_none()
    );
    assert_eq!(rt.cursor_state().expect("cursor").mapped_output, None);

    // Announce a real output through a run, mapping from inside the
    // announcement handler: the output table is populated — and cleared —
    // by the run that announced it (`clear_outputs`), so mapping after the
    // run returns would miss by design. The recorded mapping itself lives
    // in this crate's own cell, so it still reads back afterwards.
    struct OutputProbe {
        rt: wlr::Runtime,
        outputs: Vec<wlr::OutputId>,
        map_result: Option<Option<()>>,
        turns: u32,
    }
    impl wlr::OutputHandler for OutputProbe {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            self.outputs.push(output.id());
            self.map_result = Some(self.rt.map_cursor_to_output(output.id()));
        }
    }
    impl wlr::ToplevelHandler for OutputProbe {}
    impl wlr::FdHandler for OutputProbe {}
    impl wlr::LoopHandler for OutputProbe {
        fn should_stop(&mut self) -> bool {
            self.turns += 1;
            self.turns >= 4
        }
    }
    impl wlr::SeatHandler for OutputProbe {}

    let mut probe = OutputProbe {
        rt: rt.clone(),
        outputs: Vec::new(),
        map_result: None,
        turns: 0,
    };
    backend
        .run_all(&display, &mut probe, &rt, wlr::Until::Turns(4))
        .expect("run_all");
    let live = probe
        .outputs
        .into_iter()
        .next()
        .expect("headless announces one output");
    assert_eq!(
        probe.map_result,
        Some(Some(())),
        "a live output of the current run maps"
    );
    assert_eq!(rt.cursor_state().expect("cursor").mapped_output, Some(live));
}

/// M7 review (b): `constraint_state_for_surface` misses with no seat, with
/// no manager, and for an unknown surface of every role.
///
/// A live constraint's populated fields need a client that binds
/// `zwp_pointer_constraints_v1` and commits a constraint on a mapped
/// surface; `tests/common/client.rs` speaks xdg-shell, layer-shell and
/// foreign-toplevel only, so no harness commits one — the populated shape
/// is pinned instead by the `constraint_extents_saturate_rather_than_panic_or_wrap`
/// unit test on `PointerConstraintState`.
#[test]
fn constraint_state_misses_cleanly() {
    let _serial = common::headless_guard();
    common::headless_env();
    use wlr::ConstraintSurface;

    let rt = wlr::Runtime::new().unwrap();
    assert!(
        rt.constraint_state_for_surface(ConstraintSurface::Toplevel(
            wlr::ToplevelId::dangling_for_test()
        ))
        .is_none(),
        "no seat, so no constraint"
    );

    let display = wlr::Display::new().expect("display");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.create_seat(&display, "seat0").expect("seat");
    assert!(
        rt.constraint_state_for_surface(ConstraintSurface::Toplevel(
            wlr::ToplevelId::dangling_for_test()
        ))
        .is_none(),
        "no pointer-constraints manager, so no constraint"
    );

    rt.create_pointer_constraints_manager(&display)
        .expect("manager");
    assert!(
        rt.constraint_state_for_surface(ConstraintSurface::Toplevel(
            wlr::ToplevelId::dangling_for_test()
        ))
        .is_none(),
        "unknown toplevel names no surface"
    );
    assert!(
        rt.constraint_state_for_surface(ConstraintSurface::Layer(
            wlr::LayerSurfaceId::dangling_for_test()
        ))
        .is_none(),
        "unknown layer surface names no surface"
    );
    assert!(
        rt.constraint_state_for_surface(ConstraintSurface::Popup(
            wlr::PopupId::dangling_nth_for_test(1)
        ))
        .is_none(),
        "unknown popup names no surface"
    );
}

/// M7 review (c): seat rename and serial guards miss cleanly with and
/// without a seat. Null is the documented miss case for every one of these
/// (each takes null-or-live), so null probes the shared guard without any
/// hardware; only a real name on a live seat renames.
///
/// Only miss shapes are pinned here, deliberately: a live probe needs a
/// real `wl_client` (`seat_has_client`) or a real `wl_resource` (the other
/// three) that only a protocol round-trip creates, and the harness holds no
/// handle on the server-side objects its client thread bound — the client
/// proxies never cross the thread boundary as raw pointers, and the runtime
/// offers no accessor that hands one out. A dangling stand-in would be
/// unsound rather than honest (the lookups dereference what they are given
/// past the null check), so the populated shapes stay uncovered instead of
/// faked; SAFETY FIRST — no fake pointers.
#[test]
fn seat_name_and_serial_guards_miss_cleanly() {
    let _serial = common::headless_guard();
    common::headless_env();

    let rt = wlr::Runtime::new().unwrap();
    assert!(!rt.set_seat_name("seat0"), "no seat yet, so no rename");
    assert!(!rt.set_seat_name("sea\0t0"), "no seat yet, so no rename");
    // SAFETY: null is the documented miss case for each of these — a live
    // pointer is only ever read, never dereferenced, and null misses first.
    unsafe {
        assert!(!rt.seat_has_client(std::ptr::null_mut()));
        assert!(!rt.seat_has_client_for_pointer_resource(std::ptr::null_mut()));
        assert!(rt.seat_client_next_serial(std::ptr::null_mut()).is_none());
        assert!(!rt.seat_client_validate_serial(std::ptr::null_mut(), 1));
    }

    let display = wlr::Display::new().expect("display");
    rt.create_seat(&display, "seat0").expect("seat");
    assert!(rt.set_seat_name("seat1"), "a live seat renames");
    assert!(!rt.set_seat_name("sea\0t1"), "a NUL name never renames");
    // SAFETY: as above — null still misses with a live seat behind it.
    unsafe {
        assert!(!rt.seat_has_client(std::ptr::null_mut()));
        assert!(!rt.seat_has_client_for_pointer_resource(std::ptr::null_mut()));
        assert!(rt.seat_client_next_serial(std::ptr::null_mut()).is_none());
        assert!(!rt.seat_client_validate_serial(std::ptr::null_mut(), 1));
    }
}

/// M7 review (d), constraint group: a compile-pin for the commit hook's
/// signature, not a behavioral test — it never runs, so it asserts nothing
/// about delivery. An earlier revision ran a real headless run and asserted
/// the hook never fired, which read as behavioral coverage while proving
/// only that nothing happened.
/// Driving it for real needs a client that binds
/// `zwp_pointer_constraints_v1` and commits a region on a mapped surface —
/// `tests/common/client.rs` has no such helper (and the only emitters,
/// `backend.rs`'s `on_new_pointer_constraint`/`on_constraint_commit`, run
/// on client protocol traffic) — so headless cannot produce the event.
/// Behavioral coverage lives in
/// `backend::pointer_protocol_delivery_tests::constraint_and_gesture_events_route_their_ids`
/// (routing) and `backend`'s
/// `constraint_set_region_relay_announces_the_committing_constraint`
/// (real signal emission into the hook).
#[test]
fn constraint_hook_signature_compiles() {
    struct App {
        constraints: Vec<wlr::ConstraintId>,
    }
    impl wlr::SeatHandler for App {
        fn pointer_constraint_committed(&mut self, id: wlr::ConstraintId) {
            self.constraints.push(id);
        }
    }

    let app = App {
        constraints: Vec::new(),
    };
    assert!(app.constraints.is_empty());
}

/// M7 review (d), gesture group: a compile-pin for the begin/end hooks'
/// signatures, not a behavioral test — it never runs, so it asserts nothing
/// about delivery. An earlier revision ran a real headless run and asserted
/// the hooks never fired, which read as behavioral coverage while proving
/// only that nothing happened.
/// The only emitters are the hardware-pointer gesture signals
/// (`backend.rs`'s swipe/pinch/hold handlers), and headless offers no
/// pointer device through the safe API — the same limit `seat.rs`'s header
/// note records for key presses — so the event is unproducible here.
/// Behavioral coverage lives in
/// `backend::pointer_protocol_delivery_tests::constraint_and_gesture_events_route_their_ids`
/// (routing) and `backend`'s `swipe_begin_and_end_emit_without_a_manager_or_seat`,
/// `pinch_begin_and_end_emit_without_a_manager_or_seat` and
/// `hold_begin_and_end_emit_without_a_manager_or_seat` (real signal
/// emission into the hooks).
#[test]
fn gesture_hooks_signature_compiles() {
    struct App {
        began: Vec<wlr::GestureId>,
        ended: Vec<wlr::GestureId>,
    }
    impl wlr::SeatHandler for App {
        fn gesture_began(&mut self, id: wlr::GestureId) {
            self.began.push(id);
        }
        fn gesture_ended(&mut self, id: wlr::GestureId) {
            self.ended.push(id);
        }
    }

    let app = App {
        began: Vec::new(),
        ended: Vec::new(),
    };
    assert!(app.began.is_empty() && app.ended.is_empty());
}

/// M7 review (d), touch group: real injection attempted through a real run,
/// hooks asserting. `inject_touch_*` is the one injection surface this
/// group has — but it drives the client-forward path (`TouchFrame`
/// notifies), never the hardware-signal path the hooks relay, and with an
/// empty scene the hit test misses before anything emits (`None`). So the
/// vectors must stay empty *and* the injection must report its miss: both
/// halves of "attempted for real, delivered nothing" are asserted.
#[test]
fn touch_hooks_survive_real_injection_without_firing() {
    let _serial = common::headless_guard();
    common::headless_env();

    #[derive(Default)]
    struct App {
        turns: u32,
        downs: Vec<wlr::TouchId>,
        ups: Vec<wlr::TouchId>,
        cancelled: u32,
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
        fn touch_down(&mut self, id: wlr::TouchId) {
            self.downs.push(id);
        }
        fn touch_up(&mut self, id: wlr::TouchId) {
            self.ups.push(id);
        }
        fn touch_cancelled(&mut self) {
            self.cancelled += 1;
        }
    }

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_seat(&display, "seat0").expect("seat");
    runtime.enable_test_touch();

    let mut app = App::default();
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(4))
        .expect("run_all");

    // Real injection, attempted: an empty scene has no surface under the
    // cursor, so the down misses before emitting — and with no hardware
    // touch signal either, no hook may have fired.
    assert!(
        runtime.inject_touch_down(64.0, 64.0, 0, 1).is_none(),
        "nothing under the cursor, so the injection misses"
    );
    runtime.inject_touch_motion(65.0, 65.0, 0, 2);
    runtime.inject_touch_up(0, 3);
    assert!(app.downs.is_empty() && app.ups.is_empty());
    assert_eq!(app.cancelled, 0);
}

/// M7 review (d), switch group: a compile-pin for the toggle hook's
/// signature, not a behavioral test — it never runs, so it asserts nothing
/// about delivery. An earlier revision ran a real headless run and asserted
/// the hook never fired, which read as behavioral coverage while proving
/// only that nothing happened.
/// The only emitter is the switch-hardware toggle signal, and headless has
/// no switch device — nor any safe-API injection for one
/// (`note_switch_toggle` is `pub(crate)`, reachable only from that signal)
/// — so the event is unproducible here. Behavioral coverage lives in
/// `backend::touch_switch_delivery_tests::touch_and_switch_events_route_their_payloads`
/// (routing) and `backend`'s `switch_toggle_relay_records_and_announces`
/// (real signal emission into the hook).
#[test]
fn switch_hook_signature_compiles() {
    struct App {
        toggles: Vec<(wlr::SwitchId, bool)>,
    }
    impl wlr::SeatHandler for App {
        fn switch_toggled(&mut self, id: wlr::SwitchId, on: bool) {
            self.toggles.push((id, on));
        }
    }

    let app = App {
        toggles: Vec::new(),
    };
    assert!(app.toggles.is_empty());
}

/// M7 review (e): a mapped region is recorded in the snapshot.
#[test]
fn cursor_region_mapping_is_recorded_in_the_snapshot() {
    let _serial = common::headless_guard();
    common::headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");
    rt.create_seat(&display, "seat0").expect("seat");

    let region = wlr::Box2D::new(10, 20, 300, 200);
    rt.map_cursor_to_region(region)
        .expect("a seat cursor always maps a region");
    assert_eq!(
        rt.cursor_state().expect("cursor").mapped_region,
        Some(region)
    );
}

/// M7 review (e): the touch snapshot after a real inject down/up round trip.
///
/// Populating `points` needs a surface under the cursor *and* a client
/// holding a `wl_touch` resource (wlroots' `touch_point_create` refuses
/// otherwise) — an empty scene has neither, so the injection misses and the
/// snapshot stays at rest. The populated-points shape is pinned instead by
/// the `wl_touch`-client legs below (`touch_down_announces_with_id_to_a_wl_touch_client`
/// and `touch_up_announces_for_the_known_point`, driven through
/// `common::client::spawn_touch_client`); this pins the reachable half here
/// (miss shape + resting snapshot).
#[test]
fn touch_snapshot_after_inject_down_and_up() {
    let _serial = common::headless_guard();
    common::headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");
    rt.create_seat(&display, "seat0").expect("seat");
    rt.enable_test_touch();

    assert!(
        rt.inject_touch_down(64.0, 64.0, 0, 1).is_none(),
        "no surface under the cursor, so no point is created"
    );
    rt.inject_touch_up(0, 2);
    let state = rt.touch_state().expect("touch-enabled seat snapshots");
    assert!(
        state.points.is_empty(),
        "no point was ever created, so none is down"
    );
    assert!(!state.has_grab);
}

/// M7 review (e): the switch snapshot has no headless driver either.
/// `switch_state` needs a tracked switch device plus an observed toggle;
/// both arrive only on the switch-hardware signal, for which headless has
/// no source — so this pins the miss shape, and the populated shape
/// (`from_toggle`, including the lid derivation) is pinned by the
/// `switch_state_derives_lid_closed_from_type_and_position` unit test.
#[test]
fn switch_snapshot_has_no_headless_driver() {
    let _serial = common::headless_guard();
    common::headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");
    rt.create_seat(&display, "seat0").expect("seat");

    assert!(
        rt.switch_state().is_none(),
        "no switch device and no toggle observed headless"
    );
}

/// M7 review (f): destroying the seat drops every cursor reader back to a
/// miss — the id, the aggregate snapshot, and the by-id resolver alike.
#[test]
fn cursor_misses_after_seat_destroy() {
    let _serial = common::headless_guard();
    common::headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");
    rt.create_seat(&display, "seat0").expect("seat");

    let id = rt.cursor_id().expect("seat brings a cursor");
    assert!(rt.destroy_seat(), "a live seat destroys");
    assert!(rt.cursor_id().is_none(), "the id misses again");
    assert!(rt.cursor_state().is_none(), "the snapshot misses again");
    assert!(rt.try_cursor(id).is_none(), "the saved id misses again");
    assert!(!rt.destroy_seat(), "no seat left to destroy");
}

/// M7 review (g): destroying the seat from inside a dispatched handler is
/// refused, and the seat still resolves afterwards.
///
/// The refusal is a programming error — `destroy_seat`'s `debug_assert!`
/// fires in debug builds before the `false` is reached — so the attempt is
/// caught inside the handler: debug observes the gate's message, release
/// observes the `false` return, and both observe the seat still standing.
#[test]
fn destroy_seat_refused_inside_handler() {
    let _serial = common::headless_guard();
    common::headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");
    rt.create_seat(&display, "seat0").expect("seat");

    struct RefuseApp {
        rt: wlr::Runtime,
        turns: u32,
        /// `Ok(false)` in release (the refusal return), `Err(message)` in
        /// debug (the programming-error gate fires before returning).
        outcome: std::cell::RefCell<Option<Result<bool, String>>>,
    }
    impl wlr::OutputHandler for RefuseApp {
        fn new_output(&mut self, _output: &wlr::Output<'_>) {
            let attempt =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.rt.destroy_seat()));
            let recorded = attempt.map_err(|payload| {
                payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                    .unwrap_or_else(|| "(non-string panic)".to_string())
            });
            *self.outcome.borrow_mut() = Some(recorded);
        }
    }
    impl wlr::ToplevelHandler for RefuseApp {}
    impl wlr::FdHandler for RefuseApp {}
    impl wlr::LoopHandler for RefuseApp {
        fn should_stop(&mut self) -> bool {
            self.turns += 1;
            self.turns >= 4
        }
    }
    impl wlr::SeatHandler for RefuseApp {}

    let mut app = RefuseApp {
        rt: rt.clone(),
        turns: 0,
        outcome: std::cell::RefCell::new(None),
    };
    backend
        .run_all(&display, &mut app, &rt, wlr::Until::Turns(4))
        .expect("run_all");

    let outcome = app
        .outcome
        .borrow()
        .clone()
        .expect("a headless run announces an output, running the handler");
    if cfg!(debug_assertions) {
        let message = outcome.expect_err("the debug gate must fire, not destroy");
        assert!(
            message.contains("defer it until the run returns"),
            "the gate names the fix: {message}"
        );
    } else {
        assert!(
            !outcome.expect("no panic in release"),
            "the in-handler destroy is refused"
        );
    }
    assert!(
        rt.cursor_id().is_some(),
        "the refused destroy leaves the seat standing"
    );
    assert!(rt.cursor_state().is_some());
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

/// The server side of the `wl_touch` integration legs below.
///
/// One run owns the whole client lifecycle (as in `surfaces.rs`): a fresh
/// `Session` per `run_all` call unlinks every per-object listener when the
/// call returns, so a toplevel announced by one run is deaf in the next —
/// the map would fire with no listener left to hear it. The touch inject
/// therefore happens inside the same run, from `should_stop` once the map
/// has been observed: between turns every commit is fully applied (during
/// the `mapped` emission itself the scene side has not settled and the hit
/// test misses), the session is still live, and a zero serial — the
/// client's `wl_touch` resource still in flight — simply retries next turn.
/// The parked client observes the inject, exits, and `should_stop` ends the
/// single `Until::Stop` run.
struct TouchApp {
    runtime: wlr::Runtime,
    mapped: Vec<wlr::ToplevelId>,
    /// The down inject, retried from `should_stop` until it mints: `None`
    /// until then, then the inject's answer. Recorded rather than asserted
    /// anywhere near the run — the assertions below read it after the run
    /// returns.
    injected_down: Option<Option<u32>>,
    /// Misses on the retry path, counted separately so a stuck drive names
    /// the stalled leg: `None` means no surface under the cursor yet (the
    /// map has not settled), `Some(0)` means the surface resolved but the
    /// client's `wl_touch` resource is not in place yet. Read in the
    /// assertion messages below after the run returns.
    miss_no_surface: u32,
    miss_no_resource: u32,
    /// Server-side point ids read back synchronously right after the minted
    /// down — and right after the up when `inject_up` is set, in the same
    /// turn, before any flush, client exit, or disconnect can disturb them.
    /// (After the run returns the client has disconnected and the run has
    /// dispatched the disconnect, so a post-run snapshot can no longer
    /// prove anything about the drive.)
    points_after_drive: Option<Vec<i32>>,
    /// When set, a motion for the same point follows a minted down
    /// immediately — proving the motion forward reaches the wire. Asserted
    /// via the client's observed motions only in the up leg: the down leg's
    /// client exits on the down itself, so a motion still in flight there
    /// stays unasserted by design rather than by gap.
    drive_motion: bool,
    /// When set, the up for the same point follows a minted down
    /// immediately — the down/up round trip as one atomic drive.
    inject_up: bool,
    /// The client thread, owned here so [`wlr::LoopHandler::should_stop`]
    /// can end the single run once the client observed what it waits for.
    client: Option<std::thread::JoinHandle<common::client::TouchClientEvents>>,
}

impl wlr::OutputHandler for TouchApp {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        output
            .enable_with_preferred_mode()
            .expect("headless output must enable with its preferred mode");
        self.runtime
            .init_output(output)
            .expect("headless output must initialise for rendering");
    }
}

impl wlr::ToplevelHandler for TouchApp {
    fn mapped(&mut self, toplevel: &wlr::Toplevel<'_>) {
        self.mapped.push(toplevel.id());
    }
}

impl wlr::SeatHandler for TouchApp {}
impl wlr::FdHandler for TouchApp {}
impl wlr::LoopHandler for TouchApp {
    fn should_stop(&mut self) -> bool {
        // Inject between turns, never inside `mapped`: during the map
        // emission the scene side has not settled (the hit test misses
        // there), while here every commit has been fully applied. Retried
        // every turn until it mints: `None` means no surface under the
        // cursor yet, `Some(0)` means the surface resolved but the client's
        // `wl_touch` resource is not in place yet — its get_touch
        // round-trip is still in flight — so the next turn tries again and
        // each miss is counted on its own leg. Failed attempts create
        // nothing, so retrying with the same id is safe.
        if !self.mapped.is_empty() && self.injected_down.is_none() {
            match self.runtime.inject_touch_down(10.0, 10.0, 7, 1) {
                Some(serial) if serial != 0 => {
                    self.injected_down = Some(Some(serial));
                    if self.drive_motion {
                        self.runtime.inject_touch_motion(12.0, 12.0, 7, 2);
                    }
                    if self.inject_up {
                        self.runtime.inject_touch_up(7, 3);
                    }
                    self.points_after_drive = Some(
                        self.runtime
                            .touch_state()
                            .map(|state| state.points.iter().map(|point| point.id).collect())
                            .unwrap_or_default(),
                    );
                }
                Some(_) => {
                    self.miss_no_resource += 1;
                }
                None => {
                    self.miss_no_surface += 1;
                }
            }
        }
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

/// A touch down through a real `wl_touch`-holding client: the server grows a
/// point with the injected id, and the client observes the down with that id.
///
/// This is the integration half of the unit `touch_down_without_a_cursor`
/// gate pin: there the hit and the forward-and-announce tail stay undriven
/// for lack of a scene and a `wl_touch`-holding client; here both exist — a
/// mapped surface under the cursor and a client holding `wl_touch` — so the
/// down resolves, forwards, and announces with its id on both sides of the
/// wire.
#[test]
fn touch_down_announces_with_id_to_a_wl_touch_client() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    runtime.create_seat(&display, "seat0").expect("seat");
    // Advertise touch before the client binds, so its seat carries the
    // capability whose `wl_touch` resource lets the server grow a point.
    runtime.enable_test_touch();
    let socket = display.add_socket_auto().expect("socket");

    let mut app = TouchApp {
        runtime: runtime.clone(),
        mapped: Vec::new(),
        injected_down: None,
        miss_no_surface: 0,
        miss_no_resource: 0,
        points_after_drive: None,
        drive_motion: false,
        inject_up: false,
        client: Some(common::client::spawn_touch_client(&socket)),
    };
    // One run: the client's map, the `should_stop` inject once the map is
    // observed, the flushes that carry the down to the parked client, and
    // finally the client's own exit (via `should_stop`) once it observed
    // the down. Both sides are deadline-bounded (the client's park, the
    // run's end on client exit), so a stuck peer fails assertions rather
    // than hanging the suite.
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Stop)
        .expect("run_all");

    assert_eq!(app.mapped.len(), 1, "the touch client's surface must map");
    let serial = app
        .injected_down
        .expect("the mapped handler must have attempted the inject")
        .expect("mapped surface under (10, 10)");
    assert_ne!(
        serial, 0,
        "a down for a wl_touch-holding client must mint a serial"
    );
    assert_eq!(
        app.points_after_drive,
        Some(vec![7]),
        "the server must track the injected point"
    );

    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");
    assert!(
        events.transport_error.is_none(),
        "the touch park must see no transport failure, got {:?} \
         (drive misses: no-surface={} no-resource={})",
        events.transport_error,
        app.miss_no_surface,
        app.miss_no_resource
    );
    assert_eq!(
        events.downs,
        vec![7],
        "the client must observe the down with its id \
         (drive misses: no-surface={} no-resource={})",
        app.miss_no_surface,
        app.miss_no_resource
    );
}

/// A touch up for the known point: the server clears the point, and the
/// client observes the up with the down's id.
///
/// The integration half of the unit `touch_up_for_an_unknown_point` pin,
/// whose known-with-client positive needs exactly this — a live point with
/// a client, which only a `wl_touch`-holding client plus an inject can make.
#[test]
fn touch_up_announces_for_the_known_point() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_seat(&display, "seat0").expect("seat");
    runtime.enable_test_touch();
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = TouchApp {
        runtime: runtime.clone(),
        mapped: Vec::new(),
        injected_down: None,
        miss_no_surface: 0,
        miss_no_resource: 0,
        points_after_drive: None,
        drive_motion: true,
        inject_up: true,
        client: Some(common::client::spawn_touch_client_until_up(&socket)),
    };
    // One run, as for the down leg: the `should_stop` drive injects the down
    // and the up back-to-back once the down mints, the flushes carry both
    // to the parked client, and the client's exit on the up ends the run.
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Stop)
        .expect("run_all");

    assert_eq!(app.mapped.len(), 1, "the touch client's surface must map");
    assert!(
        app.injected_down
            .expect("the drive must have attempted the inject")
            .is_some(),
        "mapped surface under (10, 10)"
    );
    assert_eq!(
        app.points_after_drive,
        Some(vec![]),
        "the up must clear the server-side point"
    );

    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");
    assert!(
        events.transport_error.is_none(),
        "the touch park must see no transport failure, got {:?} \
         (drive misses: no-surface={} no-resource={})",
        events.transport_error,
        app.miss_no_surface,
        app.miss_no_resource
    );
    assert_eq!(
        events.downs,
        vec![7],
        "the client must observe the down with its id \
         (drive misses: no-surface={} no-resource={})",
        app.miss_no_surface,
        app.miss_no_resource
    );
    assert_eq!(
        events.motions,
        vec![7],
        "the client must observe the driven motion with the point's id — \
         the forward, not just the server-side point \
         (drive misses: no-surface={} no-resource={})",
        app.miss_no_surface,
        app.miss_no_resource
    );
    assert_eq!(
        events.ups,
        vec![7],
        "the client must observe the up for the known point"
    );
}

/// The cancel leg has no headless driver, so this pins its reachable setup
/// and documents the gap instead of faking the event.
///
/// What runs here is real: a `wl_touch`-holding client maps, the server
/// grows a point for it, and the point is still tracked after a flush —
/// the exact precondition a cancel would need. What cannot run is the
/// cancel itself: nothing headless emits it. The hardware path needs a
/// `wlr_touch.events.cancel` emission, and headless has no touch device to
/// emit one (no virtual-touch protocol exists, so `on_new_input` has no
/// touch arm); the inject path has no cancel either (`TouchFrame::send_cancel`
/// is `pub(crate)`, and `inject_touch_*` offers down/motion/up only). A
/// fabricated server-side point cannot stand in: it names no client, and
/// the cancel notify addresses the point's client. Until a headless cancel
/// source exists, the `Cancelled` arm stays covered by the unit
/// `send_cancel_reports_each_miss_distinctly` gate pin plus these two
/// integration legs, never by a real `wl_touch.cancel` on the wire.
#[test]
fn touch_cancel_has_no_headless_driver() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_seat(&display, "seat0").expect("seat");
    runtime.enable_test_touch();
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = TouchApp {
        runtime: runtime.clone(),
        mapped: Vec::new(),
        injected_down: None,
        miss_no_surface: 0,
        miss_no_resource: 0,
        points_after_drive: None,
        drive_motion: false,
        inject_up: false,
        client: Some(common::client::spawn_touch_client(&socket)),
    };
    // One run, as for the down leg: the `should_stop` down is the cancel
    // precondition, and the client's exit on it ends the run.
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Stop)
        .expect("run_all");

    assert_eq!(app.mapped.len(), 1, "the touch client's surface must map");
    assert!(
        app.injected_down
            .expect("the drive must have attempted the inject")
            .is_some(),
        "mapped surface under (10, 10)"
    );
    assert_eq!(
        app.points_after_drive,
        Some(vec![7]),
        "the cancel precondition — a live point with a client — must hold"
    );
    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");
    assert_eq!(
        events.downs,
        vec![7],
        "the cancel precondition — a live point with a client — must hold"
    );
    assert!(
        events.transport_error.is_none(),
        "the touch park must see no transport failure, got {:?}",
        events.transport_error
    );
    assert_eq!(
        events.cancels, 0,
        "nothing headless emits a cancel, so none may arrive"
    );
}
