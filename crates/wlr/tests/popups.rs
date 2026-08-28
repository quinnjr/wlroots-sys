//! xdg-popup, against a real headless compositor with no client.
//!
//! Same shape as `layers.rs`: what is provable without a client library is that
//! the additive handler methods really are additive, that every by-id operation
//! misses on an id no popup was ever given, and that the public types behave.
//! Anything that needs a live client — placement, flipping, chains, grab
//! dismissal, focus restore — is P2's `compositor/tests/popups.rs` against the
//! harness, which drives a real `xdg_popup` end to end.

/// A `ToplevelHandler` written against 0.20.27, with an empty body, must still
/// compile and still be usable in 0.20.28. That is the whole additivity claim
/// of this release, and it is a compile-time claim, so the test that asserts it
/// is a type that exists.
struct LegacyHandler;

impl wlr::ToplevelHandler for LegacyHandler {}

/// A handler that overrides every new method, proving the signatures are what
/// the contract froze and that a `Popup<'_>` is usable from inside one.
#[derive(Default)]
struct PopupHandler {
    seen: Vec<String>,
}

impl wlr::ToplevelHandler for PopupHandler {
    fn new_popup(&mut self, popup: &wlr::Popup<'_>) {
        self.seen.push(format!("new {:?}", popup.id()));
    }
    fn popup_initial_commit(&mut self, popup: &wlr::Popup<'_>) {
        self.seen.push(format!("commit {:?}", popup.parent()));
    }
    fn popup_mapped(&mut self, id: wlr::PopupId) {
        self.seen.push(format!("mapped {id:?}"));
    }
    fn popup_unmapped(&mut self, id: wlr::PopupId) {
        self.seen.push(format!("unmapped {id:?}"));
    }
    fn popup_reposition(&mut self, popup: &wlr::Popup<'_>) {
        self.seen
            .push(format!("reposition {:?}", popup.reposition_token()));
    }
    fn popup_destroyed(&mut self, id: wlr::PopupId) {
        self.seen.push(format!("destroyed {id:?}"));
    }
}

#[test]
fn the_popup_handler_methods_are_additive_and_overridable() {
    let _legacy = LegacyHandler;
    let handler = PopupHandler::default();
    assert!(handler.seen.is_empty());
}

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`. See `toplevels.rs`'s
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

/// A `run_all` over a headless backend with a popup-aware handler installed
/// must start, dispatch and stop cleanly. No client connects, so no popup is
/// ever announced — what this proves is that the six new events are wired
/// through `deliver_all` and that installing the handler changes nothing about
/// the ordinary lifecycle. The *delivery* of a real popup event is P2's
/// harness-driven coverage.
#[test]
fn a_run_with_a_popup_handler_starts_and_stops_cleanly() {
    headless_env();

    #[derive(Default)]
    struct App {
        turns: u32,
    }

    impl wlr::OutputHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::FdHandler for App {}

    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            self.turns += 1;
            self.turns >= 4
        }
    }

    impl wlr::ToplevelHandler for App {
        fn new_popup(&mut self, popup: &wlr::Popup<'_>) {
            // Never reached without a client; here so the method is live code
            // rather than a default, which is what makes `deliver_all`'s arm
            // reachable at all.
            let _ = popup.id();
        }
    }

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg shell");

    let mut app = App::default();
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(4))
        .expect("run_all");
}
