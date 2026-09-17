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

use std::cell::Cell;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, Once, OnceLock};

#[allow(dead_code)]
pub mod client;

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS`/`WLR_RENDERER` are set exactly
/// once, before any test in this binary calls `Backend::autocreate`.
///
/// Does NOT acquire [`headless_guard`]: client threads call the `spawn_*`
/// helpers while the test thread holds the guard, so locking here would
/// deadlock. Callers that mutate the environment must already hold the guard
/// (every display-creating test does).
#[allow(dead_code)]
pub fn headless_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once::call_once` runs these writes at most once, before any
        // `Display::new` in this binary creates the backend that reads them.
        // Every display-creating test holds `headless_guard` across its whole
        // body, so no libwayland `getenv` in this process races these writes.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            std::env::set_var("WLR_RENDERER", "pixman");
        }
    });
}

static HEADLESS_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

thread_local! {
    /// How many times this thread has acquired [`headless_guard`] without
    /// dropping. Nonzero means the thread already owns the process-wide mutex,
    /// so a nested [`headless_guard`] must not lock it again.
    static HOLD_COUNT: Cell<usize> = const { Cell::new(0) };
}

/// Holds the process-wide environment/display serialisation lock.
///
/// Reentrant per thread: the outermost acquisition locks the mutex, nested
/// ones (e.g. [`headless_env`] called while the test already holds the guard)
/// only bump the hold count. Dropping the outermost guard unlocks.
#[allow(dead_code)]
pub struct HeadlessGuard {
    _inner: Option<MutexGuard<'static, ()>>,
}

impl Drop for HeadlessGuard {
    fn drop(&mut self) {
        HOLD_COUNT.with(|c| c.set(c.get().saturating_sub(1)));
        // `_inner` drops after this, releasing the mutex only when the
        // outermost guard goes away; nested guards hold `None`.
    }
}

/// Serializes display/backend bring-up across the tests in one binary.
/// libwayland-server holds process-global state, so two `Display::new()`
/// calls racing on different test threads abort with `data is non-NULL
/// with zero alloc`. Hold this for the whole test body.
///
/// Reentrant per thread (nested holds only bump the count), but it must
/// never be acquired on a spawned client thread while a test thread may
/// hold it — so the `spawn_*` helpers and everything they call stay
/// lock-free.
///
/// Must stay in lockstep with `crates/wlr/src/test_support.rs`'s
/// `test_display_guard` (the `--lib` counterpart): the two cannot share an
/// implementation across the `cfg(test)` lib / integration-crate boundary,
/// so both copies are poison-tolerant for the same reason.
#[allow(dead_code)]
pub fn headless_guard() -> HeadlessGuard {
    let depth = HOLD_COUNT.with(|c| {
        let depth = c.get();
        c.set(depth + 1);
        depth
    });
    if depth == 0 {
        let inner = HEADLESS_MUTEX
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        HeadlessGuard {
            _inner: Some(inner),
        }
    } else {
        HeadlessGuard { _inner: None }
    }
}

/// Points `XDG_RUNTIME_DIR` at a fresh per-process temp directory and returns
/// it.
///
/// Does NOT acquire [`headless_guard`]: spawned client threads call this via
/// the `spawn_*` helpers while the test thread holds the guard, so locking
/// here would deadlock. The test must already hold the guard (every
/// display-creating test does) before the first call.
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
        // SAFETY: `OnceLock::get_or_init` runs this at most once, before any
        // `Display::new` binds a socket under it, and every display-creating
        // test holds `headless_guard` across its whole body, so no libwayland
        // `getenv` in this process races this write.
        DIR_PATH.set(path.clone()).ok();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &path);
            atexit(cleanup_runtime_dir);
        }
        path
    })
    .clone()
}

/// Bound for blocking socket I/O inside the spawned client threads.
///
/// Without it a stuck hop leaves `roundtrip` blocked forever, the thread never
/// finishes, and CI hangs where it should fail: the read call returns
/// `TimedOut`/`WouldBlock` after ten seconds, `roundtrip` surfaces the `Err`,
/// and the thread panics with the round-trip's message.
// Shared harness: not every test binary drives a client, so binaries that
// never connect would warn without the allow (see module docs).
#[allow(dead_code)]
pub const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Connect to the Wayland socket at `path` with [`IO_TIMEOUT`] read/write
/// bounds, for handing to `Connection::from_socket` on a spawned thread.
// Shared harness: see `IO_TIMEOUT` above.
#[allow(dead_code)]
pub fn connect_socket(path: &std::path::Path) -> std::os::unix::net::UnixStream {
    let stream = std::os::unix::net::UnixStream::connect(path).expect("connect to wayland socket");
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .expect("set read timeout on wayland socket");
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set write timeout on wayland socket");
    stream
}

/// Create a `0o600` shm backing file at `path` with length `len`.
///
/// Read-write, not `File::create`'s write-only: the server mmaps the fd with
/// `PROT_READ`, and mapping a write-only fd fails `EACCES` (libwayland then
/// rejects the pool with "Failed to create memory mapping"). `create_new` so
/// a stale path fails instead of being adopted, with mode `0o600` so no other
/// user can read the pixels.
// Shared harness: see `IO_TIMEOUT` above.
#[allow(dead_code)]
pub fn create_shm_backing(path: &std::path::Path, len: u64) -> std::fs::File {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .expect("create shm backing file");
    file.set_len(len).expect("size shm backing file");
    file
}

/// Unique backing path for a per-test shm file or listen socket, under
/// [`isolated_runtime_dir`].
///
/// The pid alone is predictable on a shared `/tmp` and a fixed stem collides
/// when one binary maps twice, so the name mixes in per-process entropy from
/// `nonce()` alongside the pid. `tag` only names the kind (e.g. the socket
/// name plus a role suffix); characters outside `[A-Za-z0-9-_.]` are replaced
/// so a socket name can never escape the directory. Callers create with
/// `create_new(true)` and mode `0o600`, unlink any stale path before binding,
/// and remove the file when the client thread exits. Signature-stable on
/// purpose: [`isolated_runtime_dir`] keeps returning the directory itself.
// Shared harness: see `IO_TIMEOUT` above.
#[allow(dead_code)]
pub fn shm_path_for(tag: &str) -> PathBuf {
    let mut safe = String::with_capacity(tag.len());
    for c in tag.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            safe.push(c);
        } else {
            safe.push('_');
        }
    }
    isolated_runtime_dir().join(format!(
        "wlr-rs-shm-{}-{}-{safe}",
        std::process::id(),
        nonce()
    ))
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
