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
//! `wlr_output_swapchain_manager_prepare`, which takes backend-commit states
//! from the backend-commit milestone and is not wrapped yet; until it is,
//! `get_swapchain` is e2e-only (icedtea harness).

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

/// The manager lifecycle every other call depends on: init on construction,
/// finish on drop, and apply as a no-op with nothing pending — all without a
/// trap or abort through C.
#[test]
fn swapchain_manager_lifecycle() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    let manager = SwapchainManager::new(&backend);
    // Fresh manager, no prepare since construction: nothing pending, so apply
    // must be a no-op rather than a use of uninitialised state.
    manager.apply();
    // Drop runs `wlr_output_swapchain_manager_finish`; a double-free or an
    // init/finish mismatch would trap here or under Miri, not silently pass.
    drop(manager);
}
