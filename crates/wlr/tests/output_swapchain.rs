//! Output swapchain manager lifecycle, against a real headless backend.
//!
//! This integration binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local); the setup itself is `common::headless_env`,
//! shared with the other output test binaries.
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

mod common;

use common::headless_env;
use wlr::{Backend, Display, Runtime, SwapchainManager};

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
    manager.apply().expect("apply on a live backend");
    // Drop runs `wlr_output_swapchain_manager_finish`. Under plain `cargo
    // test` this proves no trap on the empty-list path; reaping real pending
    // swapchains needs a prepare and is M13-blocked (see the module docs).
    drop(manager);
}
