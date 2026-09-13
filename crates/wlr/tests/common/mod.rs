//! Shared helpers for the `wlr` integration-test binaries.
//!
//! Each binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local), so the setup lives here instead of as per-binary
//! copies: a skew between copies presents as backend flakiness.
//!
//! Every binary uses the subset of these helpers it needs: one that only
//! creates a `Display` never calls `headless_env`, and one without a
//! display-creating test never calls `headless_guard`. That is by design, so
//! the per-binary dead-code lint is silenced on each helper individually
//! rather than module-wide — a module-wide `#![allow(dead_code)]` would also
//! hide genuinely dead helpers from every binary at once.

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, Once, OnceLock};

#[allow(dead_code)]
pub mod client;

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS`/`WLR_RENDERER` are set exactly
/// once, before any test in this binary calls `Backend::autocreate`.
#[allow(dead_code)]
pub fn headless_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once::call_once` means the writes below run at most once,
        // and in practice they run before any `Display::new` in this binary
        // creates the backend that reads them. It does *not* make
        // `set_var`/`getenv` racing impossible: tests may call `headless_env`
        // without holding `headless_guard`, so a `getenv` on another thread
        // is not excluded by this `Once` alone.
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
///
/// Must stay in lockstep with `crates/wlr/src/test_support.rs`'s
/// `test_display_guard` (the `--lib` counterpart): the two cannot share an
/// implementation across the `cfg(test)` lib / integration-crate boundary,
/// so both copies are poison-tolerant for the same reason.
#[allow(dead_code)]
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
/// `XDG_RUNTIME_DIR`, so it must be set before [`Display::new`] runs, not merely
/// before the client connects: the server binds the socket as soon as
/// `add_socket_auto` returns, and [`client::spawn`] resolves the same
/// directory when it turns that name into the socket path it hands to
/// `wayland_client::Connection::from_socket`.
///
/// Idempotent per process — the first caller sets it, every later one gets the
/// same directory — because the value is process-global and two tests racing to
/// repoint it would each strand the other's client. The tests that create a
/// display are serialized by [`headless_guard`], so the first call wins before
/// any server binds.
///
/// The directory is private (mode `0700`) with a non-predictable suffix and is
/// removed best-effort when the process exits. `create_dir` rather than
/// `create_dir_all` so a pre-existing path fails instead of being adopted, and
/// no parent is traversed.
#[allow(dead_code)]
pub fn isolated_runtime_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("wlr-test-{}-{}", std::process::id(), nonce()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("create isolated XDG_RUNTIME_DIR");
        // SAFETY: `OnceLock::get_or_init` runs this at most once and blocks
        // every other caller on the same cell until it returns, so `set_var`
        // itself is not reentered. It does *not* exclude a concurrent `getenv`
        // on a thread that is not waiting on this cell; in practice it runs
        // before any libwayland call in this process binds or resolves a
        // socket. The cleanup below is registered once, after the directory
        // exists.
        DIR_PATH.set(path.clone()).ok();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &path);
            atexit(cleanup_runtime_dir);
        }
        path
    })
    .clone()
}

/// Per-process entropy for the runtime-dir name: pid is not enough on a shared
/// `/tmp`, so mix in the wall clock and a per-process counter.
fn nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ counter.rotate_left(32) ^ (u64::from(std::process::id()) << 16)
}

/// Removes the isolated runtime directory at process exit. Best-effort: a
/// failure here is not worth reporting from an exit hook.
extern "C" fn cleanup_runtime_dir() {
    // SAFETY: reading a `OnceLock` from an atexit hook is safe; the path was
    // written before the hook was registered. `remove_dir_all` is given the
    // exact directory this process created.
    if let Some(dir) = DIR_PATH.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

// `atexit` from libc, declared directly so no new dependency is needed: std
// links libc into every binary, so the symbol is always resolvable.
unsafe extern "C" {
    fn atexit(cb: extern "C" fn()) -> i32;
}

/// The path registered for cleanup, kept separate from [`DIR`] so the hook can
/// reach it without cloning or locking.
static DIR_PATH: OnceLock<PathBuf> = OnceLock::new();
