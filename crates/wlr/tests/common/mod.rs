//! Shared helpers for the output integration-test binaries.
//!
//! Each binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local), so the setup lives here instead of as per-binary
//! copies: a skew between copies presents as backend flakiness.

// Each binary uses the subset of these helpers it needs: one that only
// creates a `Display` never calls `headless_env`, and one without a
// display-creating test never calls `headless_guard`. That is by design,
// so silence the per-binary dead-code lint for the unused half.
#![allow(dead_code)]

use std::sync::{Mutex, MutexGuard, Once, OnceLock};

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS`/`WLR_RENDERER` are set exactly
/// once, before any test in this binary calls `Backend::autocreate`.
pub fn headless_env() {
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

static HEADLESS_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

/// Serializes display/backend bring-up across the tests in one binary.
/// libwayland-server holds process-global state, so two `Display::new()`
/// calls racing on different test threads abort with `data is non-NULL
/// with zero alloc`. Hold this for the whole test body.
pub fn headless_guard() -> MutexGuard<'static, ()> {
    HEADLESS_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
