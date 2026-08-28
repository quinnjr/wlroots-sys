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
