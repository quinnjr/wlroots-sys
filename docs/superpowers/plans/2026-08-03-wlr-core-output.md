# `wlr` Core + Output Bring-Up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `wlr` crate's safe core — borrow-scoped handles, trait dispatch, reentrancy deferral, and stable IDs — ending at a headless backend that renders a frame.

**Architecture:** Consumers implement handler traits on one state struct. Handlers receive `&Output<'h>` bound to the dispatch call, so a reference cannot escape — dangling is a compile error, not a convention. Long-lived state belongs to the consumer, keyed by `OutputId`, which comes from a `wlr_addon` attached to the C object so it self-cleans on destroy. wlroots emits signals synchronously from inside API calls, so the dispatcher detects reentrancy and defers the inner event until the outer handler returns.

**Tech Stack:** Rust 2024, `wlr-sys` 0.20 (this branch's minor), libwayland, `trybuild` for compile-fail tests.

**Version selection is a branch, not a feature.** `develop` carries `wlr` 0.20.x
bound to `wlr-sys` 0.20; older wlroots minors live on the existing `support/*`
branches, each with its own `wlr` minor. Cargo cannot resolve a manifest that
lists two `wlr-sys` versions — its `links` uniqueness check runs at resolution
across all dependency edges, not the activated feature set — so mutually
exclusive version features are not available. See the spec's "Why not cargo
features".

**Spec:** `docs/superpowers/specs/2026-08-03-wlr-safe-wrapper-design.md`

## Global Constraints

Every task's requirements implicitly include these.

- **MSRV 1.88**, `edition = "2024"`, `license = "MIT"` — inherit via `edition.workspace = true` etc. Verify with `cargo +1.88 check -p wlr`. (Not `--all-features`: `wlr` has no features to combine, and `wlr-sys`'s are its own crate's business.)
- **Crate lives at `crates/wlr/`** in the existing workspace. `members = ["crates/*"]` picks it up automatically; no workspace manifest edit needed.
- **`cargo fmt --all --check` and `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` must pass** at every commit. CI enforces both.
- **Every `unsafe` block carries a `// SAFETY:` comment** stating why the invariant holds. This is a `-sys`-adjacent crate; the comments are the deliverable.
- **Never expose a way to construct a handle outside dispatch.** Handle constructors are `pub(crate)`. This is the crate's entire safety claim.
- **wlroots requires a running display for most calls.** Tasks 1–5 are unit-testable without one; Tasks 6–8 need the headless backend.
- Work on branch `feature/wlr-safe-wrapper`, already created from `develop`.

---

### Task 1: Crate scaffolding and version selection

**Files:**
- Create: `crates/wlr/Cargo.toml`
- Create: `crates/wlr/src/lib.rs`
- Create: `crates/wlr/src/sys.rs`
- Create: `crates/wlr/README.md`

**Interfaces:**
- Consumes: nothing (first task).
- Produces: `wlr::sys` — a private-to-crate re-export of `wlr-sys`. Every later task writes `use crate::sys;` and never names `wlr_sys` directly, so the indirection stays available if a future branch needs it.

- [ ] **Step 1: Write the failing test**

Create `crates/wlr/tests/version_selection.rs`:

```rust
//! This branch's `wlr` binds this branch's wlroots, and says so at runtime.
//!
//! Version selection is a branch, not a feature: cargo cannot resolve a
//! manifest listing two `wlr-sys` minors, because its `links` uniqueness check
//! runs across every dependency edge rather than the activated feature set.
//! What remains to test is that the `wlr-sys` we linked really is the minor
//! this branch claims — a mismatched path dependency or a stale lockfile would
//! otherwise go unnoticed until a symbol failed to resolve.

#[test]
fn linked_wlroots_is_this_branchs_minor() {
    let (major, minor) = wlr::wlroots_version();

    assert_eq!(major, 0, "wlroots is 0.x");
    assert_eq!(
        minor, 20,
        "this branch binds wlroots 0.20; a different minor means the wlr-sys \
         dependency does not match the branch"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p wlr --test version_selection`
Expected: FAIL — `error: no matching package named 'wlr'` (the crate does not exist yet).

- [ ] **Step 3: Write minimal implementation**

Create `crates/wlr/Cargo.toml`:

```toml
[package]
name = "wlr"
version = "0.1.0"
description = "Safe bindings to wlroots"
keywords = ["wayland", "wlroots", "compositor"]
categories = ["api-bindings", "gui"]
readme = "README.md"
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version.workspace = true

[dependencies]
# This branch binds wlroots 0.20, so it names the workspace member directly.
# A `version` alone would resolve the published copy instead, putting two
# packages with `links = "wlroots"` in the graph and breaking the whole
# workspace. Older wlroots minors live on `support/*` branches, each with its
# own `wlr` minor — cargo cannot resolve a manifest listing two of them.
wlr-sys = { version = "0.20", path = "../wlr-sys" }

[dev-dependencies]
trybuild = "1.0"
```

Create `crates/wlr/src/sys.rs`:

```rust
//! `wlr-sys`, re-exported under one name.
//!
//! Every module writes `use crate::sys;` rather than naming `wlr_sys` directly.
//! The indirection costs nothing and keeps a single edit point if a branch ever
//! needs to bind its `-sys` crate differently.

pub(crate) use wlr_sys::*;
```

Create `crates/wlr/src/lib.rs`:

```rust
//! Safe bindings to wlroots.
//!
//! See `docs/superpowers/specs/2026-08-03-wlr-safe-wrapper-design.md` for the
//! ownership model this crate is built around.

pub(crate) mod sys;

/// The wlroots version this build of `wlr` binds, as `(major, minor)`.
///
/// Read from `wlr-sys`'s own header constants rather than from this crate's
/// version, so a dependency that does not match the branch is observable
/// instead of silent.
pub fn wlroots_version() -> (u32, u32) {
    (sys::WLR_VERSION_MAJOR, sys::WLR_VERSION_MINOR)
}
```

Create `crates/wlr/README.md`:

```markdown
# wlr

Safe bindings to [wlroots](https://gitlab.freedesktop.org/wlroots/wlroots),
built on [`wlr-sys`](https://crates.io/crates/wlr-sys).

`wlr`'s minor version tracks the wlroots minor it binds, so pick the one
matching your system:

| `wlr` | wlroots | Packaged by |
|---|---|---|
| `0.20` | 0.20 | Arch |
| `0.19` | 0.19 | Ubuntu 26.04 |
| `0.17` | 0.17 | Ubuntu 24.04 |
| `0.15` | 0.15 | Ubuntu 22.04 |

```toml
wlr = "0.20"
```

The API is the same across all four, so moving between them is a version
change rather than a code change.
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p wlr --test version_selection`
Expected: PASS — 1 test.

Then confirm the crate did not break the rest of the workspace, which is the
failure mode the path dependency exists to prevent:
Run: `cargo test --workspace`
Expected: PASS — `wlr-sys`'s existing suites plus `wlr`'s. A `links` error
naming `wlroots` here means `wlr-sys` is being pulled from the registry
alongside the workspace member.

- [ ] **Step 5: Commit**

```bash
git add crates/wlr
git commit -m "feat(wlr): crate scaffolding and wlroots version selection"
```

---

### Task 2: Error type

**Files:**
- Create: `crates/wlr/src/error.rs`
- Modify: `crates/wlr/src/lib.rs`
- Test: `crates/wlr/src/error.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing.
- Produces: `wlr::Error` (enum, `#[non_exhaustive]`), `wlr::Result<T> = std::result::Result<T, Error>`. Every fallible constructor in Tasks 6–8 returns `Result<T>`.

- [ ] **Step 1: Write the failing test**

Add to `crates/wlr/src/error.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_names_the_failed_operation() {
        let e = Error::Create("wlr_backend_autocreate");
        assert_eq!(e.to_string(), "wlr_backend_autocreate failed");
    }

    #[test]
    fn error_is_a_std_error() {
        fn assert_error<T: std::error::Error>() {}
        assert_error::<Error>();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p wlr error::tests`
Expected: FAIL — `error[E0433]: failed to resolve: use of undeclared crate or module 'error'`.

- [ ] **Step 3: Write minimal implementation**

Create `crates/wlr/src/error.rs` (above the test module):

```rust
//! Failure reporting.
//!
//! wlroots signals failure by returning null or `false` and provides no detail,
//! so `Error` names the operation that failed and nothing more. Inventing detail
//! would be less truthful than admitting the C API does not supply it.

use std::fmt;

/// An operation that wlroots reported as failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A wlroots constructor returned null. The payload is the C function name.
    Create(&'static str),
    /// A wlroots operation returned `false`. The payload is the C function name.
    Operation(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Create(op) | Error::Operation(op) => write!(f, "{op} failed"),
        }
    }
}

impl std::error::Error for Error {}

/// Shorthand for results from this crate.
pub type Result<T> = std::result::Result<T, Error>;
```

Add to `crates/wlr/src/lib.rs`, after the `sys` module declaration:

```rust
mod error;

pub use error::{Error, Result};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p wlr error::tests`
Expected: PASS — 2 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/wlr/src/error.rs crates/wlr/src/lib.rs
git commit -m "feat(wlr): error type naming the failed wlroots operation"
```

---

### Task 3: Stable IDs backed by `wlr_addon`

**Files:**
- Create: `crates/wlr/src/id.rs`
- Modify: `crates/wlr/src/lib.rs`
- Test: `crates/wlr/src/id.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Consumes: `crate::sys`.
- Produces:
  - `pub struct OutputId(u64)` — `Copy + Clone + PartialEq + Eq + Hash + Debug`.
  - `pub(crate) unsafe fn attach_id(set: *mut sys::wlr_addon_set) -> u64` — attaches a fresh id addon, returns the id. Panics if one is already attached.
  - `pub(crate) unsafe fn find_id(set: *const sys::wlr_addon_set) -> Option<u64>`.

**Why an addon:** the pointer value is not a stable identity, because wlroots can reuse an address after free. A `wlr_addon` is destroyed by wlroots at exactly the right moment, so there is no side table to go stale. `wlr_output`, `wlr_surface`, `wlr_buffer`, `wlr_scene_node`, `wlr_output_layer` and `wlr_drm_syncobj_timeline` all carry an addon set; `wlr_addon`'s own `owner`/`link` fields are `WLR_PRIVATE`, so only `wlr_addon_init`/`find`/`finish` may touch it.

- [ ] **Step 1: Write the failing test**

Add to `crates/wlr/src/id.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the id addon against a standalone `wlr_addon_set`. This needs
    /// no display, backend or output — `wlr_addon_set_init` works on any set.
    #[test]
    fn ids_are_unique_stable_and_self_cleaning() {
        // SAFETY: `set` is a live, exclusively-owned value for this scope, and
        // is finished before it drops.
        unsafe {
            let mut set = std::mem::zeroed::<sys::wlr_addon_set>();
            sys::wlr_addon_set_init(&raw mut set);

            assert_eq!(find_id(&raw const set), None, "empty set has no id");

            let a = attach_id(&raw mut set);
            assert_eq!(find_id(&raw const set), Some(a), "id is retrievable");
            assert_eq!(find_id(&raw const set), Some(a), "and stable across lookups");

            let mut other = std::mem::zeroed::<sys::wlr_addon_set>();
            sys::wlr_addon_set_init(&raw mut other);
            let b = attach_id(&raw mut other);
            assert_ne!(a, b, "ids are unique across objects");

            // Finishing the set runs our destroy hook and frees the addon.
            sys::wlr_addon_set_finish(&raw mut set);
            sys::wlr_addon_set_finish(&raw mut other);
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p wlr id::tests`
Expected: FAIL — `error[E0433]: failed to resolve: use of undeclared crate or module 'id'`.

- [ ] **Step 3: Write minimal implementation**

Create `crates/wlr/src/id.rs` (above the test module):

```rust
//! Stable object identity.
//!
//! A raw pointer is not an identity: wlroots may reuse an address after free, so
//! a pointer compared across a destroy can alias a different object. Instead a
//! monotonic id is attached to the C object with `wlr_addon`, wlroots' own
//! mechanism for data whose lifetime is bound to an object. wlroots runs our
//! destructor at exactly the right moment, so nothing has to be swept.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::sys;

/// Identifies an output for as long as the consumer chooses to remember it.
///
/// Storable, comparable and hashable — unlike a handle, which cannot escape the
/// handler it was passed to. Ids are never reused within a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub(crate) u64);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Our addon payload: a `wlr_addon` header followed by the id.
///
/// `#[repr(C)]` with the addon first so `container_of!` can recover the payload
/// from the `*mut wlr_addon` wlroots hands to the destroy hook.
#[repr(C)]
struct IdAddon {
    addon: sys::wlr_addon,
    id: u64,
}

/// `wlr_addon_interface` holds raw pointers, so it is not `Sync` by default.
/// Wrapping it lets us hold one immutable instance for the process.
struct AddonImpl(sys::wlr_addon_interface);

// SAFETY: the contents are never mutated after initialisation, and the `name`
// pointer targets a `'static` C string.
unsafe impl Sync for AddonImpl {}

static ID_ADDON_IMPL: AddonImpl = AddonImpl(sys::wlr_addon_interface {
    name: c"wlr-rs-object-id".as_ptr(),
    destroy: Some(id_addon_destroy),
});

/// Called by wlroots when the owning object is destroyed.
unsafe extern "C" fn id_addon_destroy(addon: *mut sys::wlr_addon) {
    // SAFETY: wlroots only invokes this for addons we registered, all of which
    // are the `addon` field of a boxed `IdAddon`.
    unsafe {
        let payload: *mut IdAddon = wlr_sys_container_of(addon);
        sys::wlr_addon_finish(addon);
        drop(Box::from_raw(payload));
    }
}

/// Recover the `IdAddon` from its embedded `wlr_addon`.
///
/// A local helper rather than `wlr-sys`'s `container_of!`, because the macro is
/// exported from whichever versioned crate is selected and this keeps the
/// version-specific path out of the call site.
unsafe fn wlr_sys_container_of(addon: *mut sys::wlr_addon) -> *mut IdAddon {
    // SAFETY: `addon` points at the `addon` field of a live `IdAddon`, which is
    // `#[repr(C)]` with that field first, so the offset is zero.
    addon.cast::<IdAddon>()
}

/// Attach a fresh id to `set` and return it.
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live
/// object, and must not already carry one of our id addons.
pub(crate) unsafe fn attach_id(set: *mut sys::wlr_addon_set) -> u64 {
    // SAFETY: caller guarantees `set` is live and initialised.
    unsafe {
        assert!(
            find_id(set.cast_const()).is_none(),
            "an id addon is already attached to this object"
        );

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let payload = Box::into_raw(Box::new(IdAddon {
            addon: std::mem::zeroed(),
            id,
        }));

        sys::wlr_addon_init(
            &raw mut (*payload).addon,
            set,
            (&raw const ID_ADDON_IMPL).cast::<c_void>(),
            &raw const ID_ADDON_IMPL.0,
        );
        id
    }
}

/// Retrieve the id attached to `set`, if any.
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live object.
pub(crate) unsafe fn find_id(set: *const sys::wlr_addon_set) -> Option<u64> {
    // SAFETY: caller guarantees `set` is live and initialised.
    unsafe {
        let addon = sys::wlr_addon_find(
            set,
            (&raw const ID_ADDON_IMPL).cast::<c_void>(),
            &raw const ID_ADDON_IMPL.0,
        );
        if addon.is_null() {
            return None;
        }
        Some((*wlr_sys_container_of(addon)).id)
    }
}
```

Add to `crates/wlr/src/lib.rs`:

```rust
mod id;

pub use id::OutputId;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p wlr id::tests -- --nocapture`
Expected: PASS — 1 test. If it segfaults, the `owner`/`impl` pair passed to `wlr_addon_find` does not match `wlr_addon_init`; both must use `&raw const ID_ADDON_IMPL` and `&raw const ID_ADDON_IMPL.0` respectively.

- [ ] **Step 5: Commit**

```bash
git add crates/wlr/src/id.rs crates/wlr/src/lib.rs
git commit -m "feat(wlr): stable object ids via wlr_addon"
```

---

### Task 4: Dispatcher with reentrancy deferral

**Files:**
- Create: `crates/wlr/src/dispatch.rs`
- Modify: `crates/wlr/src/lib.rs`
- Test: `crates/wlr/src/dispatch.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Consumes: `crate::OutputId`.
- Produces:
  - `pub(crate) enum Event { OutputFrame(OutputId), OutputDestroyed(OutputId), NewOutput(OutputId) }`
  - `pub(crate) struct Dispatcher<S> { .. }` with `Dispatcher::new(state: *mut S)`, and `pub(crate) fn emit(&self, ev: Event, deliver: fn(&mut S, Event))`.
- Later tasks call `emit` from C callbacks and supply a `deliver` fn that routes to handler traits.

**Why:** wlroots emits signals synchronously from inside API calls — certainly on every destroy path. A handler that destroys an object re-enters dispatch while `&mut S` is live, which aliases `&mut` and is UB. Deferring the inner event is the only option that is always sound and never panics.

- [ ] **Step 1: Write the failing test**

Add to `crates/wlr/src/dispatch.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records delivery order, and re-enters the dispatcher from inside a
    /// handler — exactly what wlroots does when a handler destroys an object.
    struct Recorder {
        seen: Vec<Event>,
        reenter_with: RefCell<Option<Event>>,
        dispatcher: *const Dispatcher<Recorder>,
    }

    fn deliver(state: &mut Recorder, ev: Event) {
        state.seen.push(ev);
        if let Some(inner) = state.reenter_with.borrow_mut().take() {
            // SAFETY: the dispatcher outlives the test body.
            unsafe { (*state.dispatcher).emit(inner, deliver) };
        }
    }

    #[test]
    fn reentrant_events_are_deferred_until_the_outer_handler_returns() {
        let mut state = Recorder {
            seen: Vec::new(),
            reenter_with: RefCell::new(Some(Event::OutputDestroyed(OutputId(2)))),
            dispatcher: std::ptr::null(),
        };
        let d = Dispatcher::new(&raw mut state);
        state.dispatcher = &raw const d;

        // SAFETY: `state` outlives `d` for the duration of this call.
        unsafe { d.emit(Event::OutputFrame(OutputId(1)), deliver) };

        assert_eq!(
            state.seen,
            vec![Event::OutputFrame(OutputId(1)), Event::OutputDestroyed(OutputId(2))],
            "the inner event must arrive after the outer handler returns, not during it"
        );
    }

    #[test]
    fn non_reentrant_events_dispatch_immediately() {
        let mut state = Recorder {
            seen: Vec::new(),
            reenter_with: RefCell::new(None),
            dispatcher: std::ptr::null(),
        };
        let d = Dispatcher::new(&raw mut state);
        state.dispatcher = &raw const d;

        // SAFETY: as above.
        unsafe {
            d.emit(Event::NewOutput(OutputId(7)), deliver);
            d.emit(Event::OutputFrame(OutputId(7)), deliver);
        }

        assert_eq!(
            state.seen,
            vec![Event::NewOutput(OutputId(7)), Event::OutputFrame(OutputId(7))]
        );
        assert!(!d.is_dispatching(), "flag must be clear once dispatch unwinds");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p wlr dispatch::tests`
Expected: FAIL — `error[E0433]: failed to resolve: use of undeclared crate or module 'dispatch'`.

- [ ] **Step 3: Write minimal implementation**

Create `crates/wlr/src/dispatch.rs` (above the test module):

```rust
//! Event delivery, and the reentrancy guard that makes it sound.
//!
//! One `&mut S` reaches handlers. wlroots emits signals synchronously from
//! inside API calls, so a handler that destroys an object re-enters dispatch
//! while that `&mut S` is still live — which aliases `&mut` and is undefined
//! behaviour. The dispatcher detects reentrancy and queues the inner event,
//! draining the queue once the outer handler returns.
//!
//! Two consequences fall out of that and are deliberate:
//!
//! 1. Deferred events carry an [`OutputId`], never a handle, because the object
//!    may be destroyed before delivery. Delivery re-resolves and drops silently
//!    if it is gone.
//! 2. Anything wlroots requires the compositor to complete *before* a callback
//!    returns cannot be deferred. Those paths call handlers directly and say so.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use crate::OutputId;

/// An event awaiting delivery.
///
/// Carries ids rather than handles precisely because a deferred event may name
/// an object that no longer exists by the time it is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    NewOutput(OutputId),
    OutputFrame(OutputId),
    OutputDestroyed(OutputId),
}

/// Routes events to handler traits, one at a time.
pub(crate) struct Dispatcher<S> {
    state: *mut S,
    in_dispatch: Cell<bool>,
    deferred: RefCell<VecDeque<Event>>,
}

impl<S> Dispatcher<S> {
    pub(crate) fn new(state: *mut S) -> Self {
        Self {
            state,
            in_dispatch: Cell::new(false),
            deferred: RefCell::new(VecDeque::new()),
        }
    }

    /// True while a handler is running.
    pub(crate) fn is_dispatching(&self) -> bool {
        self.in_dispatch.get()
    }

    /// Deliver `ev`, or queue it if a handler is already running.
    ///
    /// # Safety
    ///
    /// The `*mut S` this dispatcher was built with must still be valid and must
    /// not be aliased by any live reference.
    pub(crate) unsafe fn emit(&self, ev: Event, deliver: fn(&mut S, Event)) {
        if self.in_dispatch.get() {
            self.deferred.borrow_mut().push_back(ev);
            return;
        }

        self.in_dispatch.set(true);

        // SAFETY: the flag above guarantees no other `&mut S` is live, and the
        // caller guarantees the pointer is valid.
        unsafe { deliver(&mut *self.state, ev) };

        // Drain whatever the handler queued. `pop_front` borrows only for the
        // statement, so a handler may queue more while we deliver.
        loop {
            let next = self.deferred.borrow_mut().pop_front();
            match next {
                // SAFETY: as above — still inside the guarded region.
                Some(ev) => unsafe { deliver(&mut *self.state, ev) },
                None => break,
            }
        }

        self.in_dispatch.set(false);
    }
}
```

Add to `crates/wlr/src/lib.rs`:

```rust
mod dispatch;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p wlr dispatch::tests`
Expected: PASS — 2 tests. The first proves ordering: the inner event must appear *after* the outer, never interleaved.

- [ ] **Step 5: Commit**

```bash
git add crates/wlr/src/dispatch.rs crates/wlr/src/lib.rs
git commit -m "feat(wlr): dispatcher with reentrancy deferral"
```

---

### Task 5: Borrow-scoped `Output` handle and the compile-fail proof

**Files:**
- Create: `crates/wlr/src/output.rs`
- Create: `crates/wlr/tests/compile_fail.rs`
- Create: `crates/wlr/tests/ui/output_escapes_handler.rs`
- Create: `crates/wlr/tests/ui/output_escapes_handler.stderr`
- Modify: `crates/wlr/src/lib.rs`

**Interfaces:**
- Consumes: `crate::sys`, `crate::id::{find_id, OutputId}`.
- Produces:
  - `pub struct Output<'h>` — `#[repr(transparent)]`, constructor `pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_output) -> Output<'h>`.
  - `Output::id(&self) -> OutputId`, `Output::name(&self) -> Option<String>`, `Output::commit(&self) -> Result<()>`.

**Why this task exists:** the crate's entire safety claim is that a handle cannot outlive its handler. That is an invariant, so it gets a test — a compile-fail test, since the failure mode is "this compiles when it must not".

- [ ] **Step 1: Write the failing test**

Create `crates/wlr/tests/compile_fail.rs`:

```rust
//! The crate's central safety claim, tested rather than asserted.
//!
//! `Output<'h>` is bound to the dispatch call that produced it. If this test
//! ever passes compilation, handles can escape handlers and every guarantee in
//! the crate is void.

#[test]
fn handles_cannot_escape_their_handler() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/output_escapes_handler.rs");
}
```

Create `crates/wlr/tests/ui/output_escapes_handler.rs`:

```rust
use wlr::Output;

/// Stands in for a handler: it receives a borrow-scoped handle.
fn handler<'h>(out: &Output<'h>, sink: &mut Vec<&'h Output<'h>>) {
    // Storing the handle beyond the call must not compile.
    sink.push(out);
}

fn main() {
    let mut sink: Vec<&Output<'_>> = Vec::new();
    let _ = &mut sink;
    let _ = handler;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p wlr --test compile_fail`
Expected: FAIL — `unresolved import 'wlr::Output'`, because `Output` does not exist yet. (trybuild will also report a missing `.stderr` file; both are expected at this point.)

- [ ] **Step 3: Write minimal implementation**

Create `crates/wlr/src/output.rs`:

```rust
//! Borrow-scoped output handles.
//!
//! An `Output` is valid only for the handler call that produced it. The lifetime
//! `'h` is what enforces that, and the constructor is `pub(crate)` so a consumer
//! cannot manufacture one with a lifetime of their choosing. A handle that
//! escapes a handler is therefore a compile error, not a documented rule.
//!
//! Anything a consumer needs to remember goes in their own state, keyed by
//! [`OutputId`].

use std::ffi::CStr;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::id::{find_id, OutputId};
use crate::{sys, Error, Result};

/// A wlroots output, borrowed for the duration of a handler call.
#[repr(transparent)]
pub struct Output<'h> {
    raw: NonNull<sys::wlr_output>,
    _scope: PhantomData<&'h ()>,
}

impl<'h> Output<'h> {
    /// Wrap a raw output for the duration of a handler call.
    ///
    /// # Safety
    ///
    /// `raw` must be a live `wlr_output` carrying one of our id addons, and the
    /// returned handle must not outlive the callback it was created for.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_output) -> Output<'h> {
        Output {
            raw: NonNull::new(raw).expect("wlroots handed us a null output"),
            _scope: PhantomData,
        }
    }

    pub(crate) fn as_ptr(&self) -> *mut sys::wlr_output {
        self.raw.as_ptr()
    }

    /// This output's stable identity, safe to store beyond the handler.
    pub fn id(&self) -> OutputId {
        // SAFETY: the handle's lifetime guarantees the output is live, and an id
        // addon is attached when the output is first seen (Task 7).
        let id = unsafe { find_id(&raw const (*self.raw.as_ptr()).addons) };
        OutputId(id.expect("output has no id addon; it was not registered"))
    }

    /// The output's name, as reported by the backend.
    pub fn name(&self) -> Option<String> {
        // SAFETY: the handle's lifetime guarantees the output is live. wlroots
        // may leave `name` null before the output is configured.
        unsafe {
            let name = (*self.raw.as_ptr()).name;
            if name.is_null() {
                return None;
            }
            Some(CStr::from_ptr(name).to_string_lossy().into_owned())
        }
    }

    /// Commit the output's pending state.
    ///
    /// wlroots replaced the implicit pending-state model with an explicit
    /// `wlr_output_state` part-way through the versions this project supports:
    /// 0.15 has only `wlr_output_commit`, 0.19 and later only
    /// `wlr_output_commit_state`, and 0.17 carries both during the transition.
    /// This branch binds 0.20, so it uses the newer call; the `support/*`
    /// branches differ *inside this method* and keep the signature identical,
    /// which is what lets a consumer move between them by changing a version.
    pub fn commit(&self) -> Result<()> {
        // SAFETY: the handle's lifetime guarantees the output is live. The
        // state is initialised before use and finished before it drops, as
        // wlroots requires.
        unsafe {
            let mut state = std::mem::zeroed::<sys::wlr_output_state>();
            sys::wlr_output_state_init(&raw mut state);
            let ok = sys::wlr_output_commit_state(self.raw.as_ptr(), &raw const state);
            sys::wlr_output_state_finish(&raw mut state);

            if ok {
                Ok(())
            } else {
                Err(Error::Operation("wlr_output_commit_state"))
            }
        }
    }
}
```

Add to `crates/wlr/src/lib.rs`:

```rust
mod output;

pub use output::Output;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p wlr --test compile_fail`
Expected: FAIL the first time, with trybuild printing the actual compiler output and writing `tests/ui/output_escapes_handler.stderr` under `wip/`. Copy it into place:

```bash
cp crates/wlr/wip/output_escapes_handler.stderr crates/wlr/tests/ui/
```

Re-run: `cargo test -p wlr --test compile_fail`
Expected: PASS — the UI test's expected stderr now matches. Read the captured stderr and confirm the error is a **lifetime** error (`borrowed value does not live long enough` or `lifetime may not live long enough`), not a type or import error. If it is the latter, the test is passing for the wrong reason and must be rewritten.

- [ ] **Step 5: Commit**

```bash
git add crates/wlr/src/output.rs crates/wlr/src/lib.rs crates/wlr/tests
git commit -m "feat(wlr): borrow-scoped Output handle with compile-fail proof"
```

---

### Task 6: `Display` and `EventLoop`

**Files:**
- Create: `crates/wlr/src/display.rs`
- Modify: `crates/wlr/src/lib.rs`
- Test: `crates/wlr/tests/display.rs`

**Interfaces:**
- Consumes: `crate::{sys, Error, Result}`.
- Produces:
  - `pub struct Display` — owns `*mut wl_display`, `Drop` calls `wl_display_destroy`.
  - `Display::new() -> Result<Display>`.
  - `Display::event_loop(&self) -> EventLoop<'_>`.
  - `pub struct EventLoop<'d>` with `EventLoop::as_ptr(&self) -> *mut sys::wl_event_loop` (`pub(crate)`) and `EventLoop::dispatch(&self, timeout_ms: i32) -> Result<()>`.

- [ ] **Step 1: Write the failing test**

Create `crates/wlr/tests/display.rs`:

```rust
//! A display can be created, dispatched and torn down.

#[test]
fn display_creates_and_dispatches() {
    let display = wlr::Display::new().expect("wl_display_create failed");
    let loop_ = display.event_loop();

    // A zero timeout returns immediately whether or not anything was ready; we
    // only care that dispatching does not fault.
    loop_.dispatch(0).expect("dispatch failed");
}

#[test]
fn display_is_dropped_without_leaking() {
    for _ in 0..8 {
        let display = wlr::Display::new().expect("wl_display_create failed");
        drop(display);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p wlr --test display`
Expected: FAIL — `error[E0433]: could not find 'Display' in 'wlr'`.

- [ ] **Step 3: Write minimal implementation**

Create `crates/wlr/src/display.rs`:

```rust
//! The Wayland display and its event loop.
//!
//! `Display` is one of the few things this crate genuinely owns, so it is one of
//! the few places RAII applies: `Drop` destroys it. Everything reachable *from*
//! a display is owned by wlroots and must not be dropped by us.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::{sys, Error, Result};

/// An owned Wayland display.
pub struct Display {
    raw: NonNull<sys::wl_display>,
}

impl Display {
    /// Create a display.
    pub fn new() -> Result<Self> {
        use sys::wayland_sys::ffi_dispatch;
        use sys::wayland_sys::server::*;

        // SAFETY: no arguments, no preconditions. `ffi_dispatch!` so this links
        // whether or not wayland-sys was built with its `dlopen` feature.
        let raw = unsafe {
            ffi_dispatch!(sys::wayland_sys::server::wayland_server_handle(), wl_display_create,)
        };
        let raw = NonNull::new(raw).ok_or(Error::Create("wl_display_create"))?;
        Ok(Display { raw })
    }

    /// The display's event loop.
    pub fn event_loop(&self) -> EventLoop<'_> {
        use sys::wayland_sys::ffi_dispatch;
        use sys::wayland_sys::server::*;

        // SAFETY: `self` is live, so its display is.
        let raw = unsafe {
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_display_get_event_loop,
                self.raw.as_ptr()
            )
        };
        EventLoop {
            raw: NonNull::new(raw).expect("display has no event loop"),
            _display: PhantomData,
        }
    }

    pub(crate) fn as_ptr(&self) -> *mut sys::wl_display {
        self.raw.as_ptr()
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        use sys::wayland_sys::ffi_dispatch;
        use sys::wayland_sys::server::*;

        // SAFETY: we own this display and destroy it exactly once.
        unsafe {
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_display_destroy,
                self.raw.as_ptr()
            )
        };
    }
}

/// The display's event loop, borrowed from the [`Display`].
pub struct EventLoop<'d> {
    raw: NonNull<sys::wl_event_loop>,
    _display: PhantomData<&'d Display>,
}

impl<'d> EventLoop<'d> {
    pub(crate) fn as_ptr(&self) -> *mut sys::wl_event_loop {
        self.raw.as_ptr()
    }

    /// Dispatch pending events. `timeout_ms` of 0 returns immediately.
    pub fn dispatch(&self, timeout_ms: i32) -> Result<()> {
        // SAFETY: the borrow guarantees the display, and so the loop, is live.
        let rc = unsafe {
            sys::wayland_sys::ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_event_loop_dispatch,
                self.raw.as_ptr(),
                timeout_ms
            )
        };
        if rc >= 0 {
            Ok(())
        } else {
            Err(Error::Operation("wl_event_loop_dispatch"))
        }
    }
}
```

Add to `crates/wlr/src/lib.rs`:

```rust
mod display;

pub use display::{Display, EventLoop};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p wlr --test display`
Expected: PASS — 2 tests.

If linking fails on `wl_display_create`, the `wayland_sys` re-export path is wrong: `wlr-sys` re-exports it as `wlr_sys::wayland_sys`, and calls go through `ffi_dispatch!` when the `dlopen` feature is on. Mirror the pattern in `wlr-sys`'s `examples/headless.rs`.

- [ ] **Step 5: Commit**

```bash
git add crates/wlr/src/display.rs crates/wlr/src/lib.rs crates/wlr/tests/display.rs
git commit -m "feat(wlr): owned Display and borrowed EventLoop"
```

---

### Task 7: `Backend`, output registration, and handler traits

**Files:**
- Create: `crates/wlr/src/backend.rs`
- Create: `crates/wlr/src/handler.rs`
- Modify: `crates/wlr/src/lib.rs`
- Modify: `crates/wlr/src/dispatch.rs`
- Test: covered by Task 8's integration test (this task has no standalone runtime test; its correctness is observable only with a running backend).

**Interfaces:**
- Consumes: `Display`, `EventLoop`, `Dispatcher`, `Event`, `attach_id`, `Output`.
- Produces:
  - `pub trait OutputHandler` with defaulted `new_output`, `frame`, `destroyed`.
  - `pub struct Backend<'d>` with `Backend::autocreate(loop_: &EventLoop<'d>) -> Result<Backend<'d>>`, `Backend::start(&self) -> Result<()>`, and `Backend::run<S: OutputHandler>(&self, display: &Display, state: &mut S, iterations: u32) -> Result<()>`.

**Note on `run`:** this slice takes an iteration count rather than looping forever, so the integration test terminates. A blocking `run` belongs in a later spec alongside signal handling.

- [ ] **Step 1: Write the failing test**

The observable behaviour needs a live backend, so the test is Task 8's. For this task, write the type-level check that the trait is object-safe-free and defaulted — add to `crates/wlr/src/handler.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A consumer implementing nothing at all must still satisfy the trait.
    struct Minimal;
    impl OutputHandler for Minimal {}

    #[test]
    fn every_handler_method_is_defaulted() {
        fn accepts<S: OutputHandler>(_: &S) {}
        accepts(&Minimal);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p wlr handler::tests`
Expected: FAIL — `error[E0433]: failed to resolve: use of undeclared crate or module 'handler'`.

- [ ] **Step 3: Write minimal implementation**

Create `crates/wlr/src/handler.rs`:

```rust
//! Handler traits.
//!
//! Consumers implement these on one state struct, which the dispatcher hands to
//! every handler as `&mut S`. Every method is defaulted, so a consumer
//! implements only what they use.

use crate::{Output, OutputId};

/// Output lifecycle and frame events.
pub trait OutputHandler {
    /// A new output was attached. The handle is valid only for this call;
    /// remember [`Output::id`] if you need to refer to it later.
    fn new_output(&mut self, output: &Output<'_>) {
        let _ = output;
    }

    /// It is a good time to render a frame for this output.
    ///
    /// wlroots expects rendering to happen before this returns, so this is one
    /// of the paths that is never deferred.
    fn frame(&mut self, output: &Output<'_>) {
        let _ = output;
    }

    /// The output is gone. Only the id is passed, because there is no longer an
    /// object to borrow.
    fn destroyed(&mut self, id: OutputId) {
        let _ = id;
    }
}
```

Create `crates/wlr/src/backend.rs`:

```rust
//! Backend creation and the wiring from wlroots signals to handler traits.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::dispatch::{Dispatcher, Event};
use crate::id::attach_id;
use crate::{sys, Display, Error, EventLoop, Output, OutputHandler, OutputId, Result};

/// A wlroots backend, owned by the display that created it.
pub struct Backend<'d> {
    raw: NonNull<sys::wlr_backend>,
    _loop: PhantomData<&'d ()>,
}

/// A listener plus the context needed to route its event.
///
/// `#[repr(C)]` with `listener` first so the C callback can recover this struct
/// from the `*mut wl_listener` wlroots passes it.
#[repr(C)]
struct Bound<S> {
    listener: sys::wl_listener,
    dispatcher: *const Dispatcher<S>,
}

impl<'d> Backend<'d> {
    /// Create whichever backend suits the environment.
    pub fn autocreate(loop_: &EventLoop<'d>) -> Result<Self> {
        // SAFETY: the borrow guarantees the loop is live. Passing null for the
        // session pointer means "do not hand back a session", which this slice
        // does not need.
        let raw = unsafe { sys::wlr_backend_autocreate(loop_.as_ptr(), std::ptr::null_mut()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_backend_autocreate"))?;
        Ok(Backend {
            raw,
            _loop: PhantomData,
        })
    }

    /// Start the backend. New outputs are announced after this returns.
    pub fn start(&self) -> Result<()> {
        // SAFETY: `self` is live.
        if unsafe { sys::wlr_backend_start(self.raw.as_ptr()) } {
            Ok(())
        } else {
            Err(Error::Operation("wlr_backend_start"))
        }
    }

    /// Wire up handlers and dispatch `iterations` turns of the event loop.
    ///
    /// Takes a count rather than blocking forever so tests terminate; a
    /// blocking loop belongs with signal handling in a later slice.
    pub fn run<S: OutputHandler>(
        &self,
        display: &Display,
        state: &mut S,
        iterations: u32,
    ) -> Result<()> {
        let dispatcher = Dispatcher::new(&raw mut *state);

        let mut new_output = Box::new(Bound {
            listener: new_listener(on_new_output::<S>),
            dispatcher: &raw const dispatcher,
        });

        // SAFETY: the backend is live, and `new_output` outlives this function
        // because it is not dropped until after the dispatch loop below.
        unsafe {
            sys::wl_signal_add(
                &raw mut (*self.raw.as_ptr()).events.new_output,
                &raw mut new_output.listener,
            );
        }

        let loop_ = display.event_loop();
        for _ in 0..iterations {
            loop_.dispatch(0)?;
        }

        // SAFETY: unlink before the boxed listener drops, exactly as a
        // compositor must.
        unsafe { remove_listener(&raw mut new_output.listener) };
        Ok(())
    }
}

fn new_listener(notify: unsafe extern "C" fn(*mut sys::wl_listener, *mut std::ffi::c_void)) -> sys::wl_listener {
    sys::wl_listener {
        link: sys::wl_list {
            prev: std::ptr::null_mut(),
            next: std::ptr::null_mut(),
        },
        notify,
    }
}

/// # Safety
///
/// `link` must be the `link` of a listener currently in a signal's list.
unsafe fn remove_listener(listener: *mut sys::wl_listener) {
    use sys::wayland_sys::ffi_dispatch;
    use sys::wayland_sys::server::*;

    // `wl_list_remove` is not re-exported by wlr-sys; it lives in wayland-sys
    // and must go through `ffi_dispatch!` so this works whether or not
    // wayland-sys was built with its `dlopen` feature. The first macro argument
    // is only evaluated in the dlopen case.
    //
    // SAFETY: caller guarantees the listener is linked.
    unsafe {
        ffi_dispatch!(
            sys::wayland_sys::server::wayland_server_handle(),
            wl_list_remove,
            &raw mut (*listener).link
        );
    }
}

/// # Safety
///
/// `l` must be the `listener` field of a live `Bound<S>`.
unsafe fn bound_of<S>(l: *mut sys::wl_listener) -> *mut Bound<S> {
    // SAFETY: `Bound<S>` is `#[repr(C)]` with `listener` first, so the offset is
    // zero. This is the same container_of pattern wlr-sys documents, with the
    // field deliberately at offset zero to keep it trivially correct.
    l.cast::<Bound<S>>()
}

unsafe extern "C" fn on_new_output<S: OutputHandler>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for listeners we registered.
    unsafe {
        let bound = bound_of::<S>(l);
        let output = data.cast::<sys::wlr_output>();

        // Give the output an identity before anyone can ask for one.
        let id = OutputId(attach_id(&raw mut (*output).addons));

        (*(*bound).dispatcher).emit(Event::NewOutput(id), deliver::<S>);
    }
}

/// Route an event to the matching handler method.
fn deliver<S: OutputHandler>(state: &mut S, ev: Event) {
    match ev {
        Event::NewOutput(id) | Event::OutputFrame(id) => {
            // Re-resolving from an id is what makes deferral safe: an object
            // destroyed between queueing and delivery simply is not found.
            let _ = id;
        }
        Event::OutputDestroyed(id) => state.destroyed(id),
    }
}
```

Add to `crates/wlr/src/lib.rs`:

```rust
mod backend;
mod handler;

pub use backend::Backend;
pub use handler::OutputHandler;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p wlr handler::tests`
Expected: PASS — 1 test.

Run: `RUSTFLAGS="-D warnings" cargo clippy -p wlr --all-targets`
Expected: clean. The `deliver` function has a deliberately incomplete `NewOutput`/`OutputFrame` arm at this point — Task 8 completes it once an id-to-pointer lookup exists.

- [ ] **Step 5: Commit**

```bash
git add crates/wlr/src/backend.rs crates/wlr/src/handler.rs crates/wlr/src/lib.rs
git commit -m "feat(wlr): backend creation, listener wiring and handler traits"
```

---

### Task 8: Frame delivery and the headless integration test

**Files:**
- Modify: `crates/wlr/src/backend.rs`
- Modify: `crates/wlr/src/output.rs`
- Create: `crates/wlr/tests/headless.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–7.
- Produces: a registry mapping `OutputId` to `*mut sys::wlr_output` so deferred events can re-resolve, and `frame`/`destroyed` delivery.

**Why a registry:** deferred events carry ids, so delivery must turn an id back into a pointer. Entries are removed on `destroy`, which fires before wlroots frees the output — so a lookup after destruction fails rather than dangling.

- [ ] **Step 1: Write the failing test**

Create `crates/wlr/tests/headless.rs`:

```rust
//! End-to-end: a headless backend announces an output and delivers a frame.
//!
//! This is the proof that the whole model works against real wlroots — handles,
//! ids, dispatch and deferral together. It needs no GPU and no seat.

use std::collections::HashMap;

#[derive(Default)]
struct App {
    outputs: HashMap<wlr::OutputId, String>,
    frames: u32,
    destroyed: Vec<wlr::OutputId>,
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        self.outputs
            .insert(output.id(), output.name().unwrap_or_default());
    }

    fn frame(&mut self, output: &wlr::Output<'_>) {
        assert!(
            self.outputs.contains_key(&output.id()),
            "frame for an output we were never told about"
        );
        self.frames += 1;
    }

    fn destroyed(&mut self, id: wlr::OutputId) {
        self.outputs.remove(&id);
        self.destroyed.push(id);
    }
}

#[test]
fn headless_backend_announces_an_output() {
    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let mut app = App::default();

    backend.start().expect("backend start");
    backend.run(&display, &mut app, 4).expect("run");

    assert!(
        !app.outputs.is_empty(),
        "the headless backend should have announced at least one output"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `WLR_BACKENDS=headless cargo test -p wlr --test headless`
Expected: FAIL — the assertion fires, because `deliver` does not yet call `new_output`.

- [ ] **Step 3: Write minimal implementation**

In `crates/wlr/src/backend.rs`, replace the `deliver` function and extend `Bound` handling. Add the registry to `Dispatcher`'s neighbours by threading it through `Bound`:

```rust
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// Maps ids back to live outputs so deferred events can re-resolve.
    ///
    /// Thread-local because wlroots is single-threaded: the display, backend and
    /// every callback run on one thread, so no lock is needed and none is taken.
    static OUTPUTS: RefCell<HashMap<OutputId, *mut sys::wlr_output>> =
        RefCell::new(HashMap::new());
}

pub(crate) fn register_output(id: OutputId, raw: *mut sys::wlr_output) {
    OUTPUTS.with(|m| m.borrow_mut().insert(id, raw));
}

pub(crate) fn unregister_output(id: OutputId) {
    OUTPUTS.with(|m| m.borrow_mut().remove(&id));
}

fn with_output<R>(id: OutputId, f: impl FnOnce(&Output<'_>) -> R) -> Option<R> {
    let raw = OUTPUTS.with(|m| m.borrow().get(&id).copied())?;
    // SAFETY: the entry is removed in the destroy handler, which wlroots runs
    // before freeing the output, so a present entry is a live output.
    let out = unsafe { Output::from_raw(raw) };
    Some(f(&out))
}

/// Route an event to the matching handler method.
///
/// Ids are resolved here rather than carried as handles, which is what makes
/// deferral sound: an output destroyed between queueing and delivery is simply
/// absent from the registry and the event is dropped.
fn deliver<S: OutputHandler>(state: &mut S, ev: Event) {
    match ev {
        Event::NewOutput(id) => {
            with_output(id, |out| state.new_output(out));
        }
        Event::OutputFrame(id) => {
            with_output(id, |out| state.frame(out));
        }
        Event::OutputDestroyed(id) => {
            unregister_output(id);
            state.destroyed(id);
        }
    }
}
```

Extend `on_new_output` to register the output and subscribe to its `frame` and `destroy` signals:

```rust
unsafe extern "C" fn on_new_output<S: OutputHandler>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for listeners we registered.
    unsafe {
        let bound = bound_of::<S>(l);
        let output = data.cast::<sys::wlr_output>();

        let id = OutputId(attach_id(&raw mut (*output).addons));
        register_output(id, output);

        // Per-output listeners are leaked deliberately: they must outlive this
        // call and are unlinked in the destroy handler, which is the only point
        // at which their lifetime is known to end.
        let frame = Box::into_raw(Box::new(Bound::<S> {
            listener: new_listener(on_frame::<S>),
            dispatcher: (*bound).dispatcher,
        }));
        sys::wl_signal_add(&raw mut (*output).events.frame, &raw mut (*frame).listener);

        let destroy = Box::into_raw(Box::new(Bound::<S> {
            listener: new_listener(on_destroy::<S>),
            dispatcher: (*bound).dispatcher,
        }));
        sys::wl_signal_add(&raw mut (*output).events.destroy, &raw mut (*destroy).listener);

        (*(*bound).dispatcher).emit(Event::NewOutput(id), deliver::<S>);
    }
}

unsafe extern "C" fn on_frame<S: OutputHandler>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: as above.
    unsafe {
        let bound = bound_of::<S>(l);
        let output = data.cast::<sys::wlr_output>();
        let Some(id) = crate::id::find_id(&raw const (*output).addons) else {
            return;
        };
        (*(*bound).dispatcher).emit(Event::OutputFrame(OutputId(id)), deliver::<S>);
    }
}

unsafe extern "C" fn on_destroy<S: OutputHandler>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: as above.
    unsafe {
        let bound = bound_of::<S>(l);
        let output = data.cast::<sys::wlr_output>();
        let Some(id) = crate::id::find_id(&raw const (*output).addons) else {
            return;
        };
        remove_listener(&raw mut (*bound).listener);
        (*(*bound).dispatcher).emit(Event::OutputDestroyed(OutputId(id)), deliver::<S>);
        drop(Box::from_raw(bound));
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `WLR_BACKENDS=headless cargo test -p wlr --test headless`
Expected: PASS — the headless backend announces one output and `app.outputs` is non-empty.

Then run the whole suite:
Run: `cargo test -p wlr`
Expected: PASS — version selection, error, id, dispatch, handler, display, compile-fail and headless.

- [ ] **Step 5: Commit**

```bash
git add crates/wlr/src/backend.rs crates/wlr/src/output.rs crates/wlr/tests/headless.rs
git commit -m "feat(wlr): frame and destroy delivery, headless integration test"
```

---

### Task 9: CI across the wlroots version matrix

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `CONTRIBUTING.md`

**Interfaces:**
- Consumes: the `wlr` crate as built by Tasks 1–8.
- Produces: CI coverage for `wlr` on this branch, and the documented procedure by which a `support/*` branch verifies its own.

**Note:** version selection is a branch, so there is no feature matrix to run here. This branch's CI proves `wlr` against wlroots 0.20; each `support/*` branch will run the same job against its own wlroots when `wlr` is cherry-picked there.

- [ ] **Step 1: Add the CI step**

First check whether `.github/workflows/ci.yml` already covers `wlr` via an existing `cargo test --workspace` step. If it does, no new test step is needed — `wlr` is a workspace member and is already built and tested. In that case, verify instead that no step uses `--workspace --all-features` in a way `wlr` breaks, and adjust only what is actually broken.

If a dedicated step is warranted, add after the existing workspace test step:

```yaml
      # `wlr` binds this branch's wlroots minor. Older minors live on support/*
      # branches, each running this same job against its own wlroots — cargo
      # cannot resolve a manifest listing two `wlr-sys` versions.
      - run: cargo test -p wlr
```

- [ ] **Step 2: Verify the workspace-wide commands still hold**

Run each of these and fix whatever `wlr`'s presence broke:

```sh
cargo test --workspace
cargo check --workspace --all-features
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
cargo fmt --all --check
```

`cargo check --workspace --all-features` is the one at risk: it is in CI today, and `--all-features` applies to every member. Confirm it passes with `wlr` in the workspace; if it does not, report what broke rather than silently dropping the step.

- [ ] **Step 3: Document the branch procedure**

Add to `containers/README.md`, under "Usage":

````markdown
### Building `wlr` against a specific wlroots

`wlr`'s minor tracks the wlroots minor it binds, and each lives on its own
branch, so verifying one means checking out that branch and using its distro's
container:

```sh
git checkout support/wlroots-0.19
docker run --rm -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/tmp/t \
  wlr-sys-ubuntu:26.04 cargo test -p wlr
```
````

Add to `CONTRIBUTING.md`, in the "Before opening a PR" section, after the existing commands:

```sh
cargo test -p wlr                      # the safe wrapper, this branch's wlroots
```

- [ ] **Step 4: Verify in a container**

The `support/*` branches do not carry `wlr` yet, so there is nothing to verify there in this slice. Instead confirm this branch's own container run is clean:

```bash
docker run --rm -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/tmp/t \
  wlr-sys-arch cargo test --workspace
```

If the Arch container image is not built locally, build it per `containers/README.md` first. If it cannot be built, report that rather than skipping the step silently.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml CONTRIBUTING.md containers/README.md
git commit -m "ci: build and test wlr across the wlroots version matrix"
```

---

## Done when

- `cargo test --workspace` passes — `wlr` alongside `wlr-sys`, with no `links` collision.
- The compile-fail test rejects an escaping handle with a *lifetime* error.
- The reentrancy test shows the inner event delivered after the outer handler, never during.
- `cargo fmt --all --check` and `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` are clean.
- `cargo +1.88 check -p wlr` passes.

## Deliberately out of scope

Seat and input, xdg-shell, layer-shell, the scene graph, renderer surface beyond output bring-up, and a blocking `run` with signal handling. Each is its own spec and plan, built on the foundation this plan validates.
