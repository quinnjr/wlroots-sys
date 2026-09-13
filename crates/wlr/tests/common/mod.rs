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

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, Once, OnceLock};

pub mod client;

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

/// Points `XDG_RUNTIME_DIR` at a fresh per-process temp directory and returns
/// it.
///
/// libwayland binds `Display::add_socket_auto`'s `wayland-N` socket *under*
/// `XDG_RUNTIME_DIR`, and `wayland_client::Connection::connect_to_env` resolves
/// `WAYLAND_DISPLAY` against the same variable. So it must be set before
/// [`Display::new`] runs, not merely before the client connects: a client that
/// read the parent environment's `XDG_RUNTIME_DIR` would look for the socket in
/// a directory the server never wrote to.
///
/// Idempotent per process — the first caller sets it, every later one gets the
/// same directory — because the value is process-global and two tests racing to
/// repoint it would each strand the other's client. The tests that create a
/// display are serialized by [`headless_guard`], so the first call wins before
/// any server binds.
pub fn isolated_runtime_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("wlr-test-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create isolated XDG_RUNTIME_DIR");
        // SAFETY: `OnceLock::get_or_init` runs this at most once and blocks
        // every other caller on the same cell until it returns, so no
        // concurrent `getenv` can observe a torn write. It happens before any
        // libwayland call in this process binds or resolves a socket.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &path);
        }
        path
    })
    .clone()
}
