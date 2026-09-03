//! Pointer axis (scroll), against a real headless compositor with no client.
//!
//! Same shape, and the same limits, as `popups.rs`: this integration binary
//! has no client library and no way to synthesise a `wlr_pointer_axis_event`
//! (`wlr-sys` is not a dev-dependency, deliberately — a test that reached past
//! the safe API would stop testing the safe API). What is provable here is
//! that `SeatHandler::pointer_axis` is additive, that it is overridable with
//! the signature the release froze, and that installing it changes nothing
//! about the ordinary run lifecycle.
//!
//! The two halves it cannot reach are covered elsewhere, both mechanically:
//!
//! * Event routing — an `Event::PointerAxis` reaching `pointer_axis` with its
//!   milli-scaled integers converted back to `f64` — is
//!   `backend::axis_delivery_tests` in the crate's own unit tests, which can
//!   build a `Session` and call `deliver_all` directly.
//! * The end-to-end proof that a *client* receives `wl_pointer.axis` and its
//!   `frame` is icedtea's `interaction_gate.rs` scroll test (contract entry
//!   P8-D74), which drives a real GTK client under the harness compositor
//!   with an injected `zwlr_virtual_pointer` — and which was `#[ignore]`d for
//!   exactly the gap this release closes.

/// A `SeatHandler` written against 0.20.28, with an empty body, must still
/// compile and still be usable in 0.20.29. That is the additivity claim of
/// this release, and it is a compile-time claim, so the test that asserts it
/// is a type that exists.
struct LegacySeatHandler;

impl wlr::SeatHandler for LegacySeatHandler {}

/// A handler that overrides the new method, proving the signature is what the
/// contract froze and that the public axis vocabulary is nameable from
/// outside the crate.
#[derive(Default)]
struct ScrollHandler {
    seen: Vec<String>,
}

impl wlr::SeatHandler for ScrollHandler {
    fn pointer_axis(
        &mut self,
        x: f64,
        y: f64,
        axis: wlr::PointerAxis,
        delta: f64,
        delta_discrete: i32,
        source: wlr::AxisSource,
        time_msec: u32,
    ) {
        self.seen.push(format!(
            "{axis:?} {delta} ({delta_discrete}) from {source:?} at ({x}, {y}) @{time_msec}"
        ));
    }
}

#[test]
fn the_pointer_axis_handler_method_is_additive_and_overridable() {
    let _legacy = LegacySeatHandler;
    let handler = ScrollHandler::default();
    assert!(handler.seen.is_empty());
}

/// The public enums are `Copy`, `Eq` and `Debug` from outside the crate —
/// which is what lets a consumer match on them, store them in its own event
/// log, and assert on them in its own tests.
#[test]
fn the_axis_vocabulary_is_usable_from_outside_the_crate() {
    let axis = wlr::PointerAxis::Vertical;
    let copied = axis;
    assert_eq!(axis, copied);
    assert_ne!(wlr::PointerAxis::Vertical, wlr::PointerAxis::Horizontal);

    assert_ne!(wlr::AxisSource::Wheel, wlr::AxisSource::Finger);
    assert_ne!(
        wlr::AxisRelativeDirection::Identical,
        wlr::AxisRelativeDirection::Inverted
    );

    assert_eq!(format!("{:?}", wlr::AxisSource::WheelTilt), "WheelTilt");
    assert_eq!(
        format!("{:?}", wlr::AxisRelativeDirection::Inverted),
        "Inverted"
    );
}

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`. See `popups.rs`'s
/// identical copy for the full argument — this is a separate integration-test
/// binary with its own environment and its own possible parallel `#[test]`
/// threads, so it needs its own `Once`.
fn headless_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once::call_once` runs this closure at most once and blocks
        // every other caller on this `Once` until it returns, so no concurrent
        // `getenv` can observe a torn write.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
        }
    });
}

/// A `run_all` over a headless backend with a scroll-aware handler installed
/// must start, dispatch and stop cleanly. No pointer is ever plugged in, so no
/// axis event is ever produced — what this proves is that the new event is
/// wired through `deliver_all` and that overriding the method changes nothing
/// about the ordinary lifecycle.
#[test]
fn a_run_with_a_scroll_handler_starts_and_stops_cleanly() {
    headless_env();

    #[derive(Default)]
    struct App {
        turns: u32,
        scrolls: u32,
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
        fn pointer_axis(
            &mut self,
            _x: f64,
            _y: f64,
            _axis: wlr::PointerAxis,
            _delta: f64,
            _delta_discrete: i32,
            _source: wlr::AxisSource,
            _time_msec: u32,
        ) {
            // Never reached without a pointer device; here so the method is
            // live code rather than a default, which is what makes
            // `deliver_all`'s arm reachable at all.
            self.scrolls += 1;
        }
    }

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_seat(&display, "seat0").expect("seat");

    let mut app = App::default();
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(4))
        .expect("run_all");

    assert_eq!(
        app.scrolls, 0,
        "a headless run with no pointer plugged in must not synthesise a \
         scroll from nowhere"
    );
}
