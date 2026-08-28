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

#[test]
fn every_by_id_popup_operation_misses_on_an_id_no_popup_was_given() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let dead = wlr::PopupId::dangling_for_test();

    assert!(runtime.popup(dead).is_none());
    assert_eq!(runtime.popup_parent(dead), None);
    assert_eq!(runtime.popup_position(dead), None);
    assert!(!runtime.popup_is_grabbing(dead));
    assert!(!runtime.configure_popup(dead, &wlr::Box2D::new(0, 0, 800, 600)));
    assert_eq!(runtime.dismiss_popup(dead), 0);
    assert!(runtime.popups_of(wlr::PopupParent::Popup(dead)).is_empty());
    assert!(
        runtime
            .popup_chain(wlr::PopupParent::Popup(dead))
            .is_empty()
    );
    assert_eq!(wlr::PopupParent::Popup(dead).root(&runtime), None);
}

/// Every distinct dangling id must miss, not just the canonical one — a
/// compositor's own tests drive several popups at once and need more than one
/// id that resolves to nothing.
#[test]
fn several_dangling_popup_ids_all_miss_and_stay_distinct() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let ids: Vec<_> = (1..=4).map(wlr::PopupId::dangling_nth_for_test).collect();
    for (i, a) in ids.iter().enumerate() {
        assert!(runtime.popup(*a).is_none());
        for b in &ids[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

/// A parent that is a live window with no popups reports an empty chain rather
/// than an error — the shape a compositor's "dismiss everything under this
/// window" path calls on every unmap.
#[test]
fn a_parent_with_no_popups_has_an_empty_chain_and_dismisses_nothing() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let window = wlr::PopupParent::Toplevel(wlr::ToplevelId::dangling_for_test());
    assert!(runtime.popups_of(window).is_empty());
    assert!(runtime.popup_chain(window).is_empty());
    assert_eq!(runtime.dismiss_popups_of(window), 0);
    assert_eq!(window.root(&runtime), Some(window));
}

/// `seat_has_explicit_grab` is called from focus paths that run before a seat
/// exists. It must answer, not dereference.
#[test]
fn asking_about_an_explicit_grab_before_there_is_a_seat_is_false() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    assert!(!runtime.seat_has_explicit_grab());
}

/// The positioner types are usable from outside the crate: a consumer building
/// its own placement policy needs to construct a `PositionerRules`-shaped
/// question and read the answer. This also pins that the public field set is
/// what the contract froze — a renamed or removed field fails to compile here.
#[test]
fn the_positioner_types_are_usable_from_outside_the_crate() {
    let c = wlr::ConstraintAdjustment::FLIP_X | wlr::ConstraintAdjustment::SLIDE_Y;
    assert!(c.contains(wlr::ConstraintAdjustment::FLIP_X));
    assert!(!c.contains(wlr::ConstraintAdjustment::FLIP_Y));

    // Every public field, named. If the contract's shape changes, this stops
    // compiling, which is the point.
    fn describe(r: &wlr::PositionerRules) -> (i32, i32, bool) {
        let _ = (
            r.anchor,
            r.gravity,
            r.constraint_adjustment,
            r.offset,
            r.parent_size,
            r.parent_configure_serial,
            r.anchor_rect,
        );
        (r.size.0, r.size.1, r.reactive)
    }
    let _ = describe as fn(&wlr::PositionerRules) -> (i32, i32, bool);

    assert_eq!(
        wlr::PositionerAnchor::BottomLeft,
        wlr::PositionerAnchor::BottomLeft
    );
    assert_ne!(wlr::PositionerGravity::Top, wlr::PositionerGravity::Bottom);
}

/// `PopupParent` is `Hash` + `Eq` because a compositor keys its own popup
/// registry by it. Pinning that here keeps a derive from being dropped.
#[test]
fn popup_parent_is_usable_as_a_map_key() {
    use std::collections::HashMap;
    let mut m: HashMap<wlr::PopupParent, u32> = HashMap::new();
    let w = wlr::PopupParent::Toplevel(wlr::ToplevelId::dangling_for_test());
    let l = wlr::PopupParent::Layer(wlr::LayerSurfaceId::dangling_for_test());
    let p = wlr::PopupParent::Popup(wlr::PopupId::dangling_for_test());
    m.insert(w, 1);
    m.insert(l, 2);
    m.insert(p, 3);
    assert_eq!(m.len(), 3);
    assert_eq!(m.get(&w), Some(&1));
    assert!(p.is_popup());
    assert!(!w.is_popup() && !l.is_popup());
}
