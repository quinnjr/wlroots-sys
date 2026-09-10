//! Input-method candidate popups, against a real headless compositor with no
//! client.
//!
//! Same shape, and the same limits, as `axis.rs` and `popups.rs`: this
//! integration binary has no client library and no way to bind
//! `zwp_input_method_v2`, let alone drive it into creating a popup surface
//! (`wlr-sys` is deliberately not a dev-dependency). What is provable here is
//! that the two popup callbacks are additive — an empty `SeatHandler` impl
//! written against the previous release still compiles, because they are
//! defaulted — that they are overridable with the signature this release
//! froze, and that installing them changes nothing about the ordinary run
//! lifecycle.
//!
//! The callbacks live on `SeatHandler`, not a trait of their own: a new
//! supertrait on `Handlers` would break every downstream consumer that did not
//! also implement it, which is not allowed in a `0.20.z` patch release (see
//! commit `f2cc8a9`, which dropped a would-be `SessionLockHandler` for exactly
//! this reason). So the additivity claim below is asserted through
//! `SeatHandler`.
//!
//! The two halves it cannot reach are covered elsewhere:
//!
//! * Event routing — an `Event::InputMethodPopupCreated`/`Destroyed` reaching
//!   the handler — rides the same `deliver_all` arm every other event uses; it
//!   is wired beside `SessionLockChanged`.
//! * The end-to-end proof that a *real* input-method popup reaches the handler
//!   and is placed in the scene is icedtea's harness tests 8-10 (contract B9),
//!   which drive a real IME client under the harness compositor.

/// A `SeatHandler` written against the release before this one, with an empty
/// body, must still compile and still be usable now: the two popup methods are
/// defaulted, so they do not appear in a legacy impl. That is the additivity
/// claim of this release, and it is a compile-time claim, so the test that
/// asserts it is a type that exists.
struct LegacyHandler;

impl wlr::SeatHandler for LegacyHandler {}

/// A handler that overrides both new methods, proving the signatures are what
/// the contract froze and that `InputPopupSurfaceId` is nameable from outside
/// the crate.
#[derive(Default)]
struct PopupHandler {
    seen: Vec<String>,
}

impl wlr::SeatHandler for PopupHandler {
    fn new_popup_surface(&mut self, popup: wlr::InputPopupSurfaceId) {
        self.seen.push(format!("created {popup:?}"));
    }

    fn popup_surface_destroyed(&mut self, popup: wlr::InputPopupSurfaceId) {
        self.seen.push(format!("destroyed {popup:?}"));
    }
}

#[test]
fn the_input_method_popup_callbacks_are_additive_and_overridable() {
    let _legacy = LegacyHandler;
    let handler = PopupHandler::default();
    assert!(handler.seen.is_empty());
}

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`. See `axis.rs`'s
/// identical copy for the full argument — this is a separate integration-test
/// binary with its own environment.
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

/// A `run_all` over a headless backend with an IME-popup-aware handler
/// installed must start, dispatch and stop cleanly. No input-method is ever
/// bound, so no popup event is ever produced — what this proves is lifecycle
/// neutrality only (overriding the methods breaks nothing about the ordinary
/// run), not event routing: a deleted `deliver_all` popup arm would still
/// pass. Routing a real popup to the handler is icedtea's harness test 8.
#[test]
fn a_run_with_an_input_method_popup_handler_starts_and_stops_cleanly() {
    headless_env();

    #[derive(Default)]
    struct App {
        turns: u32,
        popups: u32,
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
        fn new_popup_surface(&mut self, _popup: wlr::InputPopupSurfaceId) {
            // Never reached without an IME client; here so the method is live
            // code rather than a default, which is what makes `deliver_all`'s
            // arm reachable at all.
            self.popups += 1;
        }

        fn popup_surface_destroyed(&mut self, _popup: wlr::InputPopupSurfaceId) {
            self.popups += 1;
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
        app.popups, 0,
        "a headless run with no input-method bound must not synthesise a \
         popup from nowhere"
    );
}

/// The unknown-id and no-IME contracts need no Wayland client: a dangling id
/// (one no listener address can ever equal) must miss cleanly on every popup
/// accessor, and the cursor-rectangle anchor must be `None` with no IME bound.
/// A regression turning a clean `None` into a panic has this as its tripwire.
#[test]
fn unknown_popup_ids_and_no_ime_miss_cleanly() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let bogus = wlr::InputPopupSurfaceId::dangling_nth_for_test(0);

    assert!(
        runtime.input_popup_surface(bogus).is_none(),
        "unknown popup id must resolve to no surface"
    );
    assert!(
        runtime
            .send_input_popup_rectangle(
                bogus,
                wlr::Box2D {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1
                }
            )
            .is_none(),
        "rectangle send to an unknown popup id must send nothing"
    );
    assert!(
        runtime
            .add_input_popup_in_band(bogus, wlr::Band::Top)
            .is_none(),
        "placing an unknown popup id must create no node"
    );
    assert!(
        runtime.rt_debug_input_popup_node(bogus).is_none(),
        "unknown popup id must track no node"
    );
    assert!(
        runtime.focused_text_input_cursor_rectangle().is_none(),
        "no bound IME must anchor against no cursor rectangle"
    );
}
