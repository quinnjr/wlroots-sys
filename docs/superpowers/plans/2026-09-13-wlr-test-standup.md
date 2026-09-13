# wlr Test-Standup (PR-0) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the three missing test legs (client-driven, fuzzing, benchmarks) and de-duplicate the headless harness, so M9 (and every later milestone) can meet the roadmap's six-leg testing standard.

**Architecture:** `wlr` stays API-stable; this plan adds dev-dependencies, a shared client harness under `crates/wlr/tests/common/`, a standalone `fuzz/` cargo-fuzz crate excluded from the root workspace, a `criterion` bench target, and CI wiring. No wrapped `wlr` public API changes.

**Tech Stack:** Rust 2024 (MSRV 1.88), wlroots 0.20 via `wlr`/`wlr-sys`, `wayland-client` 0.31, `wayland-protocols` 0.31, `criterion`, `cargo-fuzz` on nightly.

**Spec:** `docs/superpowers/specs/2026-09-13-m9-shell-completion-design.md` (Part 0)

## Global Constraints

- MSRV floor is `rust-version = "1.88"` (root `Cargo.toml`); every new stable dependency must build on 1.88. Nightly-only tooling (fuzz) must stay out of the MSRV lane.
- Frozen `wlr-sys` hand-written API within the 0.20 line — no changes under `crates/wlr-sys/src`.
- `wlr`'s wrapped public API is unchanged by this plan (dev-dependencies, tests, benches, fuzz crate, CI only).
- The coverage audit command is `cargo xtask coverage --check`; the CI gate is `cargo test -p wlr --all-features --test coverage_audit` with `WLR_COVERAGE_ALL_FEATURES=1`.
- Workspace members are `["crates/*"]`; the fuzz crate must be added to `[workspace] exclude`, not members.
- `resolver = "3"`, edition `2024`.

---

### Task 1: Shared headless test support

**Files:**
- Modify: `crates/wlr/tests/common/mod.rs`
- Modify: every `crates/wlr/tests/*.rs` that defines a private `headless_env()` (or private headless bring-up helper).

**Interfaces:**
- Consumes: nothing.
- Produces: `common::headless_env()` (idempotently sets `WLR_BACKENDS=headless`, `WLR_HEADLESS_OUTPUTS=1`, `WLR_RENDERER=pixman`) and `common::headless_guard() -> std::sync::MutexGuard<'static, ()>` — a process-wide guard every display-creating test holds for the duration of its runtime, because libwayland-server has process-global state and the harness runs tests in parallel threads. A `headless_runtime()` helper is deliberately **not** produced: `Display`/`Backend` drop order is load-bearing and must stay at each call site.

- [ ] **Step 1: Identify the duplicates**

Run:
```bash
rg -l "fn headless_env" crates/wlr/tests/
```
Expected: ~35 files. Record the list.

- [ ] **Step 2: Extend `crates/wlr/tests/common/mod.rs`**

Keep the existing `headless_env()`; ensure it is `pub fn headless_env()` and idempotent via `std::sync::Once`. It must set all three vars under `unsafe { std::env::set_var(...) }` with the existing SAFETY comment.

Add the serialization guard — the fix for the pre-existing libwayland-server flake (`data is non-NULL with zero alloc`, reproduced at ~5% when two display-creating tests run in parallel):

```rust
use std::sync::{Mutex, MutexGuard, OnceLock};

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
```

- [ ] **Step 3: Migrate one representative file (including the guard)**

In `crates/wlr/tests/headless.rs`: delete the private `headless_env()` body, add at the top of the file:
```rust
mod common;
```
replace the inline `unsafe { std::env::set_var(...) }` block with `common::headless_env();`, and acquire the guard at the start of the test body:
```rust
let _serial = common::headless_guard();
```

- [ ] **Step 4: Verify the representative migration**

Run: `cargo test -p wlr --test headless`
Expected: PASS, same number of tests as before.

- [ ] **Step 5: Migrate the remaining files**

For each file from Step 1: delete the private `fn headless_env`, add `mod common;`, call `common::headless_env();`, and acquire `let _serial = common::headless_guard();` at the top of every test body that creates a `Display`, holding it until that runtime is dropped. Some files also carry private `argb()` helpers; leave those in place.

- [ ] **Step 6: Full test run + flake verification**

Run: `cargo test -p wlr --tests`
Expected: PASS, no `SIGABRT`.

Then prove the guard fixes the flake (this failed ~1/20 before the guard):
```bash
for i in $(seq 1 20); do cargo test -p wlr --test output_protocols -q >/dev/null 2>&1 || echo "FAIL $i"; done
```
Expected: zero `FAIL` lines.

- [ ] **Step 7: Commit**

```bash
git add crates/wlr/tests/
git commit -m "test(wlr): share headless_env across all integration test binaries"
```

---

### Task 2: Client-driven harness

**Files:**
- Modify: `crates/wlr/Cargo.toml` (dev-dependencies)
- Create: `crates/wlr/tests/common/client.rs`
- Modify: `crates/wlr/tests/common/mod.rs` (`pub mod client;`)
- Create: `crates/wlr/tests/client_harness.rs`

**Interfaces:**
- Consumes: `common::headless_env()`.
- Produces: `client::ClientState` and `client::spawn(socket: &str, drive: impl FnOnce(&mut ClientState, &wayland_client::QueueHandle<ClientState>) + Send + 'static) -> std::thread::JoinHandle<()>`; and `common::isolated_runtime_dir() -> std::path::PathBuf` (sets `XDG_RUNTIME_DIR` once, idempotently, to a fresh unique temp dir).

- [ ] **Step 1: Add dev-dependencies**

In `crates/wlr/Cargo.toml` under `[dev-dependencies]` add:
```toml
wayland-client = "0.31"
wayland-protocols = { version = "0.31", features = ["client", "unstable"] }
```
Run `cargo update -p wayland-client` if needed. Check `crates/wlr/Cargo.lock` resolves on 1.88.

- [ ] **Step 2: Write the failing seed test**

Create `crates/wlr/tests/client_harness.rs`:
```rust
mod common;

use wlr::{Backend, Display, Runtime, Until};

#[derive(Default)]
struct App {
    toplevels: usize,
}

impl wlr::OutputHandler for App {}
impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, _t: &wlr::Toplevel<'_>) {
        self.toplevels += 1;
    }
}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {}

#[test]
fn a_real_client_creates_a_toplevel_the_server_observes() {
    common::headless_env();
    let _serial = common::headless_guard();
    common::isolated_runtime_dir(); // XDG_RUNTIME_DIR must exist before the socket is bound
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let handle = common::client::spawn(&socket, |state, qh| state.create_toplevel(qh));

    let mut app = App::default();
    for _ in 0..40 {
        backend
            .run_all(&display, &mut app, &runtime, Until::Turns(50))
            .expect("run_all");
        if app.toplevels > 0 {
            break;
        }
    }
    handle.join().expect("client thread");

    assert!(app.toplevels >= 1, "server should observe the client's toplevel");
}
```
(The `ClientState` methods referenced here are defined in Step 3. `Handlers` is a blanket trait over `OutputHandler + ToplevelHandler + SeatHandler + FdHandler + LoopHandler`, so `App` implements those five directly, not `Handlers`.)

- [ ] **Step 3: Implement `crates/wlr/tests/common/client.rs`**

Provide the harness. It must:
1. Assume `common::isolated_runtime_dir()` has already set `XDG_RUNTIME_DIR` (the test calls it before `Display::new()`, so the server's `add_socket_auto` binds there and the client reuses the same value). `spawn` sets only `WAYLAND_DISPLAY`.
2. In the client thread, call `wayland_client::Connection::connect_to_env()`, `globals::registry_queue_init::<ClientState>(&conn)`, bind `xdg_wm_base` and `wl_compositor`, then run the caller's `drive` closure.
3. Expose `ClientState` with `fn create_toplevel(&mut self, qh: &QueueHandle<Self>)` that creates a `wl_surface` + `xdg_surface` + `xdg_toplevel`, attaches no buffer, and commits.
4. `spawn(socket, drive) -> JoinHandle<()>`: `unsafe { std::env::set_var("WAYLAND_DISPLAY", socket) }` inside the thread, connect, run `drive`, then `conn.flush()` and block in `conn.roundtrip()` so the server sees the requests before the thread exits.

Sketch (adjust exact `wayland-client` 0.31 names at implementation time; the module's public shape — `ClientState`, `spawn`, `common::isolated_runtime_dir` — is fixed):
```rust
use std::sync::{Arc, Mutex};
use wayland_client::{Connection, Dispatch, QueueHandle, globals::registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

pub struct ClientState {
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub wm_base: Option<xdg_wm_base::XdgWmBase>,
}

pub fn spawn(
    socket: &str,
    drive: impl FnOnce(&mut ClientState, &QueueHandle<ClientState>) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let socket = socket.to_owned();
    std::thread::spawn(move || {
        // XDG_RUNTIME_DIR was set by common::isolated_runtime_dir() before the
        // server bound its socket; the client reuses the same value, so only
        // WAYLAND_DISPLAY is set here.
        // SAFETY: env is process-global; this runs before any libwayland call
        // on this thread.
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", &socket);
        }
        let conn = Connection::connect_to_env().expect("connect");
        let (globals, mut queue) = registry_queue_init::<ClientState>(&conn).expect("registry");
        let qh = queue.handle();
        let mut state = ClientState { compositor: None, wm_base: None };
        // bind compositor + xdg_wm_base via globals.bind(...) in the registry handler
        let _ = &globals;
        drive(&mut state, &qh);
        conn.flush().expect("flush");
        let _ = queue.roundtrip(&mut state);
    })
}
```
Implement `Dispatch<wl_registry::WlRegistry, ()>`, `Dispatch<wl_compositor::WlCompositor, ()>`, `Dispatch<xdg_wm_base::XdgWmBase, ()>`, `Dispatch<xdg_surface::XdgSurface, ()>`, `Dispatch<xdg_toplevel::XdgToplevel, ()>` for `ClientState`, binding the globals in the registry handler.

Also add `common::isolated_runtime_dir()` to `tests/common/mod.rs`: idempotent via `OnceLock<PathBuf>`, creates `std::env::temp_dir().join(format!("wlr-test-{}", std::process::id()))`, `create_dir_all`s it, sets `XDG_RUNTIME_DIR` under `unsafe { std::env::set_var(...) }`, and returns the path.

- [ ] **Step 4: Run the seed test**

Run: `cargo test -p wlr --test client_harness`
Expected: PASS. If `toplevels == 0`, the client thread is not flushing/roundtripping — fix the harness, not the assertion.

- [ ] **Step 5: Commit**

```bash
git add crates/wlr/Cargo.toml crates/wlr/tests/common/ crates/wlr/tests/client_harness.rs
git commit -m "test(wlr): real wayland-client harness with a seed toplevel test"
```

---

### Task 3: Benchmark leg

**Files:**
- Modify: `crates/wlr/Cargo.toml` (`criterion` dev-dep, `[[bench]]`)
- Create: `crates/wlr/benches/dispatch.rs`
- Modify: `.github/workflows/ci.yml` (artifact job)

**Interfaces:**
- Consumes: headless setup.
- Produces: a runnable bench target named `dispatch`.

- [ ] **Step 1: Add criterion**

In `crates/wlr/Cargo.toml`:
```toml
[dev-dependencies]
criterion = "0.5"

[[bench]]
name = "dispatch"
harness = false
```

- [ ] **Step 2: Write the bench**

Create `crates/wlr/benches/dispatch.rs` with three `criterion_group` benchmarks, each paired safe-vs-raw:
1. `handle_borrow` — resolve a scene node handle by id vs reading the raw pointer directly.
2. `listener_dispatch` — one event through the observer layer vs a bare `wl_listener`.
3. `scene_op` — a scene-node mutation via the wrapper vs the raw `sys` call.
Use `criterion::{criterion_group, criterion_main, Criterion, black_box}`. Bring the headless runtime up once outside the timed loops.

- [ ] **Step 3: Run the bench once**

Run: `cargo bench -p wlr --bench dispatch -- --warm-up-time 1 --measurement-time 1`
Expected: completes and prints three benchmark groups.

- [ ] **Step 4: CI artifact job**

Add to `.github/workflows/ci.yml` a `bench` job that runs on push to `develop` only (`if: github.event_name == 'push' && github.ref == 'refs/heads/develop'`), runs `cargo bench -p wlr --bench dispatch`, and uploads `target/criterion/**` with `actions/upload-artifact`. Non-gating.

- [ ] **Step 5: Commit**

```bash
git add crates/wlr/Cargo.toml crates/wlr/benches/ .github/workflows/ci.yml
git commit -m "test(wlr): criterion benches for safe-layer dispatch overhead"
```

---

### Task 4: Fuzz leg

**Files:**
- Modify: `Cargo.toml` (`[workspace] exclude = ["fuzz"]`)
- Create: `fuzz/Cargo.toml`
- Create: `fuzz/fuzz_targets/operations.rs`
- Create: `.github/workflows/fuzz.yml`

**Interfaces:**
- Consumes: `wlr` (path dependency).
- Produces: a cargo-fuzz target `operations`.

- [ ] **Step 1: Create the standalone fuzz crate**

`fuzz/Cargo.toml`:
```toml
[package]
name = "wlr-fuzz"
version = "0.0.0"
publish = false
edition = "2021"

[package.metadata]
cargo-fuzz = true

[dependencies]
libfuzzer-sys = "0.4"
arbitrary = { version = "1", features = ["derive"] }
wlr = { path = "../crates/wlr" }

[[bin]]
name = "operations"
path = "fuzz_targets/operations.rs"
test = false
doc = false
```
Add `exclude = ["fuzz"]` to the root `[workspace]` table so `cargo build --workspace` (stable/MSRV) never builds it. Proof: `cargo metadata --format-version 1 | jq '.workspace_members'` does not list `wlr-fuzz`.

- [ ] **Step 2: Define the cumulative operation enum**

`fuzz/fuzz_targets/operations.rs` (top):
```rust
#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
enum Operation {
    CreateToplevel,
    ConfigureToplevel { width: u16, height: u16 },
    DestroyToplevel,
    CreatePopup,
    DestroyPopup,
    ConfigureOutput { enabled: bool },
    SessionLock,
    SessionUnlock,
    Activate { serial: u32 },
    // later milestones append here; the enum is cumulative.
}

fuzz_target!(|ops: Vec<Operation>| {
    // bring up headless runtime, replay ops, assert no panic.
});
```
The body brings up a headless `Display`/`Backend`/`Runtime` (ASan is the UAF oracle) and replays each operation through the safe API, tolerating `None` misses but never panicking.

- [ ] **Step 3: Build the target locally (nightly)**

Run: `cargo +nightly fuzz build operations` (from `fuzz/`)
Expected: builds. If `cargo-fuzz` is unavailable, `cargo +nightly build --manifest-path fuzz/Cargo.toml --bin operations`.

- [ ] **Step 4: Smoke-run the target**

Run: `cargo +nightly fuzz run operations -- -runs=1000`
Expected: no crash. (Run from `fuzz/`.)

- [ ] **Step 5: Nightly CI**

Create `.github/workflows/fuzz.yml`:
```yaml
name: fuzz
on:
  schedule: [{ cron: "0 3 * * *" }]
  workflow_dispatch:
jobs:
  operations:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly
      - run: cargo install cargo-fuzz
      - run: cargo +nightly fuzz run operations -- -max_total_time=300
        working-directory: fuzz
```

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml fuzz/ .github/workflows/fuzz.yml
git commit -m "test(wlr): cumulative fuzz target for stateful wrappers"
```

---

### Task 5: CI wiring and docs

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `crates/wlr/README.md`

**Interfaces:**
- Consumes: Tasks 2–4.
- Produces: nothing consumed later.

- [ ] **Step 1: Ensure the client test runs in CI**

Confirm the `test` job's `cargo test` invocation picks up `tests/client_harness.rs` with no extra config (it will, as an integration binary). If the job sets `--all-features`, verify `wayland-client` dev-dep resolves under all features.

- [ ] **Step 2: Verify MSRV lane unaffected**

Run: `cargo +1.88 test -p wlr --tests --no-run` (or the repo's MSRV command from `ci.yml`).
Expected: builds. If `wayland-client`/`wayland-protocols` exceed 1.88, pin to a compatible release and note it in the commit.

- [ ] **Step 3: Document the three legs**

In `crates/wlr/README.md`, add a short "Testing the safe layer" subsection naming the six legs, the commands (`cargo test`, `cargo bench -p wlr --bench dispatch`, `cargo +nightly fuzz run operations`), and where the client harness lives (`tests/common/client.rs`).

- [ ] **Step 4: Full gate**

Run:
```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo xtask coverage --check
```
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml crates/wlr/README.md
git commit -m "ci(wlr): wire client, bench and fuzz legs; document the test standard"
```

---

## Self-Review

- **Spec coverage:** Part 0's §0.1 client leg → Task 2; §0.2 fuzz → Task 4; §0.3 benches → Task 3; §0.4 dedupe → Task 1; §0.5 API-stability → enforced by Global Constraints. CI wiring (implied by §0.2/§0.3 and the roadmap) → Tasks 3–5.
- **Placeholder scan:** the one intentionally deferred item is the exact `wayland-client` 0.31 dispatch impls in Task 2 Step 3 — the module's public shape is fixed and the seed test is concrete, so the implementer has a verifiable target; this is the only place where exact upstream call names are left to the implementer.
- **Type consistency:** `ClientState`, `spawn`, `headless_env`, and the `operations` target name are defined once and reused consistently across tasks.
- **Open risk carried from spec:** `wayland-client`/`wayland-protocols` MSRV vs 1.88 (Task 5 Step 2 gates it).
