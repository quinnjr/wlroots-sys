//! Output swapchain manager lifecycle, against a real headless backend.
//!
//! Same shape as the other per-binary `headless_env` helpers: this
//! integration binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local).
//!
//! Only the lifecycle is covered here — construction, apply-with-nothing-
//! pending, and drop. The repaint loop the manager exists for (prepare →
//! acquire → backend commit → apply, then `get_swapchain`) needs
//! `wlr_output_swapchain_manager_prepare` (blocked on M13: `rg M13` finds
//! the ledger rows and the `SwapchainManager` docs pointing at it), so
//! `get_swapchain` and `SwapchainRef::acquire` are e2e-only until then.
//! What this file proves is the lifecycle every other call depends on:
//! a live backend constructs, apply with nothing pending is a no-op, and
//! drop finishes — without a trap or abort through C. It is smoke, not
//! state coverage: with nothing ever prepared there is no pending state
//! to read back.

use std::sync::Once;
use wlr::{Backend, Display, Runtime, SwapchainManager};

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`. See `output.rs`'s
/// identical copy for the full argument — this is a separate integration-test
/// binary with its own environment.
fn headless_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once::call_once` runs this closure at most once and blocks
        // every other caller on this `Once` until it returns, so no concurrent
        // `getenv` can observe a torn write.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            std::env::set_var("WLR_RENDERER", "pixman");
        }
    });
}

/// Applying with nothing pending is a no-op and dropping finishes: the
/// lifecycle half of the manager contract. Deliberately one behaviour per
/// test would prove no more — with nothing prepared there is no pending
/// state to distinguish init from apply from finish, so this stays a
/// single smoke test and says so.
#[test]
fn manager_apply_with_nothing_pending_is_noop() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    // A live backend constructs: `new` gates on backend liveness, so `Ok`
    // here is a real assertion, not just "did not trap".
    let manager = SwapchainManager::new(&backend).expect("manager on a live backend");
    // Fresh manager, no prepare since construction: nothing pending, so apply
    // must be a no-op rather than a use of uninitialised state.
    manager.apply();
    // Drop runs `wlr_output_swapchain_manager_finish`. Under plain `cargo
    // test` this proves no trap on the empty-list path; reaping real pending
    // swapchains needs a prepare and is M13-blocked (see the module docs).
    drop(manager);
}
