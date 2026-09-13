//! `#[cfg(test)]`-only support shared by this crate's unit tests.
//!
//! Gated behind `#[cfg(test)]` in `lib.rs`, so nothing here is compiled into,
//! or reachable from, the published library: `wlr`'s public surface is
//! unchanged by this module's existence.

use std::sync::{Mutex, MutexGuard, OnceLock};

static DISPLAY_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

/// Serializes `Display`/`Backend` bring-up across the tests in this crate's
/// `--lib` unit-test binary.
///
/// libwayland-server holds process-global state, and libtest runs `#[test]`
/// functions on parallel threads by default, so two `Display::new` calls
/// racing on different threads abort with `data is non-NULL with zero alloc`.
/// Hold this for the whole test body of every unit test that creates a
/// `Display` or a `Backend` — the unit-test counterpart of the integration
/// tests' `common::headless_guard`, and poison-tolerant for the same reason: a
/// test that panicked while holding it must not poison every later test in the
/// binary.
pub(crate) fn test_display_guard() -> MutexGuard<'static, ()> {
    DISPLAY_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
