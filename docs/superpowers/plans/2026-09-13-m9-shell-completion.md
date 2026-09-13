# wlr M9 — Shell Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reach zero `not-yet` rows tagged `milestone = "M9"` in `crates/wlr/coverage/waived.toml` by wrapping the shell-completion surface of wlroots 0.20 behind the crate's borrow-scoped-handle conventions, proven by the six-leg test standard.

**Architecture:** One keystone (a public borrow-scoped `Surface` handle + generic `wlr_surface` event delivery), then one module per protocol family following the existing `toplevel.rs`/`popup.rs`/`layer.rs` pattern: a borrow-scoped handle (or owned handle for manager-created objects), a stable id attached via `wlr_addon`, a `RuntimeInner` registry, and additive defaulted handler methods. Ledger rows that are internal/served-indirectly are reclassified; symbols needing a `wl_resource` are re-homed to M13.

**Tech Stack:** Rust 2024 (MSRV 1.88), wlroots 0.20 via `wlr`/`wlr-sys`.

**Spec:** `docs/superpowers/specs/2026-09-13-m9-shell-completion-design.md`

## Global Constraints

- **Additive public API only within 0.20.x.** No `Handlers` supertrait change; no new required trait. Surface and family events are delivered through **defaulted methods added to the existing `ToplevelHandler`** (the layer/popup precedent). *(Deviation from the spec's "new `SurfaceHandler` trait", ruled during planning: adding a trait to the `Handlers` bound is source-breaking.)*
- **Iron rule:** no `unwrap`/`expect`/`assert`/indexing in `extern "C"`-reached or handler-driven code — record in state, assert after the loop returns.
- **Frozen `wlr-sys`:** no changes under `crates/wlr-sys/src`.
- The milestone is complete only when `cargo xtask coverage` reports **zero** `not-yet` rows tagged M9; `cargo test -p wlr --all-features --test coverage_audit` gates it.
- Every new handle gets a destroy-order/UAF test; every new state machine gets a client-driven test (the harness from PR-0, `tests/common/client.rs`); M9 operations are appended to the cumulative fuzz enum.
- MSRV 1.88; `cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- Identity is always carried in `Bound` / the addon id, **never** read from a signal's `data` (wlroots emits null `data` for several surface signals).
- One patch release for the whole milestone (**0.20.36**).

### Ledger re-homes (apply in this plan's ledger sweep, Task 10)

| Symbols | New home |
|---|---|
| every `*_from_resource` (`wlr_surface_from_resource`, `wlr_xdg_surface_from_resource`, `wlr_xdg_toplevel_from_resource`, `wlr_xdg_popup_from_resource`, `wlr_xdg_positioner_from_resource`, `wlr_layer_surface_v1_from_resource`, `wlr_ext_foreign_toplevel_handle_v1_from_resource`, `wlr_region_from_resource`) | **M13** |
| `wlr_ext_foreign_toplevel_image_capture_source_manager_v1*` (4) | **M10** |
| `wlr_surface_get_content_type_v1` | **M11** |
| `wlr_compositor_set_renderer` | **M13** |

---

## Task 1: Surface handle model + generic surface events

The keystone. Everything else depends on it. Full code is given because the wiring is subtle (addon kinds, `Bound`, registry, dispatch drains).

**Files:**
- Create: `crates/wlr/src/surface.rs`
- Create: `crates/wlr/tests/surfaces.rs`
- Create: `crates/wlr/tests/ui/surface_escapes_handler.rs` (+ `.stderr`), `crates/wlr/tests/ui/surface_from_raw_is_private.rs` (+ `.stderr`)
- Modify: `crates/wlr/src/lib.rs`, `crates/wlr/src/id.rs`, `crates/wlr/src/backend.rs`, `crates/wlr/src/dispatch.rs`, `crates/wlr/src/runtime.rs`, `crates/wlr/src/handler.rs`, `crates/wlr/tests/compile_fail.rs`
- Modify: `crates/wlr/coverage/wrapped.toml`, `crates/wlr/coverage/waived.toml`

**Interfaces:**
- Consumes: `id::next_id`, `Addon::attach/find`.
- Produces: `Surface<'h>`, `SurfaceId`, `Surface::id/current_size/has_buffer/mapped`, `Runtime::surface(SurfaceId) -> Option<Surface<'_>>`, `ToplevelHandler::{surface_committed,surface_mapped,surface_unmapped,surface_destroyed,new_subsurface}`.

- [ ] **Step 1: Declare the second addon kind in `id.rs`**

Add beside the existing `ID_ADDON_IMPL` (mirroring it exactly, with a distinct name string and payload kind):

```rust
addon_kind!(
    /// The generic surface-id payload's addon kind. A distinct kind from
    /// `ID_ADDON_IMPL` so a `wlr_surface` can carry both its role id and a
    /// `SurfaceId`: `wlr_addon` keys on `(owner, impl)`, so two statics
    /// coexist on one set with no ordering or role check.
    SURFACE_ID_ADDON_IMPL: u64 = c"wlr-rs-surface-id"
);

pub(crate) unsafe fn attach_surface_id(set: *mut sys::wlr_addon_set) -> u64 {
    unsafe {
        assert!(
            find_surface_id(set.cast_const()).is_none(),
            "a surface id addon is already attached to this object"
        );
        let id = next_id();
        Addon::attach(set, SURFACE_ID_ADDON_IMPL.owner(), &SURFACE_ID_ADDON_IMPL, id);
        id
    }
}

pub(crate) unsafe fn find_surface_id(set: *const sys::wlr_addon_set) -> Option<u64> {
    unsafe { Addon::find(set, SURFACE_ID_ADDON_IMPL.owner(), &SURFACE_ID_ADDON_IMPL).as_ref().map(|a| a.data) }
}

pub(crate) unsafe fn ensure_surface_id_raw(set: *mut sys::wlr_addon_set) -> u64 {
    unsafe {
        match find_surface_id(set.cast_const()) {
            Some(id) => id,
            None => attach_surface_id(set),
        }
    }
}
```

- [ ] **Step 2: Write the failing integration test**

`crates/wlr/tests/surfaces.rs` — mirror `tests/toplevels.rs`:

```rust
mod common;

use wlr::{Backend, Display, Runtime, SurfaceId, Until};

#[derive(Default)]
struct App { surfaces: usize, destroyed: usize }

impl wlr::OutputHandler for App {}
impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, _t: &wlr::Toplevel<'_>) {}
    fn surface_mapped(&mut self, _id: SurfaceId) { self.surfaces += 1; }
    fn surface_destroyed(&mut self, _id: SurfaceId) { self.destroyed += 1; }
}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {}

#[test]
fn every_surface_operation_misses_on_a_dangling_id() {
    common::headless_env();
    let _serial = common::headless_guard();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    let mut app = App::default();
    backend.run_all(&display, &mut app, &runtime, Until::Turns(4)).expect("run_all");
    assert!(runtime.surface(SurfaceId::dangling_for_test()).is_none());
    assert_eq!(SurfaceId::dangling_for_test(), SurfaceId::dangling_for_test());
}
```

- [ ] **Step 3: Run it and watch it fail to compile**

Run: `cargo test -p wlr --test surfaces`
Expected: FAIL — `SurfaceId`, `Runtime::surface`, `surface_mapped`, `surface_destroyed` do not exist.

- [ ] **Step 4: Create `crates/wlr/src/surface.rs`**

```rust
//! Borrow-scoped `wlr_surface` handles and their stable ids.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::sys;

/// Identifies one live `wlr_surface` for as long as a consumer remembers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceId(pub(crate) u64);

impl SurfaceId {
    pub fn dangling_for_test() -> SurfaceId { SurfaceId(u64::MAX) }
    pub fn dangling_nth_for_test(n: u64) -> SurfaceId { SurfaceId(u64::MAX - n) }
}

/// A surface, borrowed for the duration of a handler call.
pub struct Surface<'h> {
    raw: NonNull<sys::wlr_surface>,
    id: SurfaceId,
    _scope: PhantomData<&'h ()>,
}

impl<'h> Surface<'h> {
    /// # Safety
    ///
    /// `raw` must be a live `wlr_surface` whose addon set carries `id`, and
    /// the handle must not outlive the callback it was created for.
    pub(crate) unsafe fn from_raw_with_id(raw: *mut sys::wlr_surface, id: SurfaceId) -> Surface<'h> {
        Surface { raw: NonNull::new(raw).expect("wlroots handed us a null surface"), id, _scope: PhantomData }
    }

    pub fn id(&self) -> SurfaceId { self.id }

    /// The surface's committed size, `(width, height)`.
    pub fn current_size(&self) -> (i32, i32) {
        // SAFETY: the handle borrows a live surface for its lifetime.
        unsafe { ((*self.raw.as_ptr()).current.width, (*self.raw.as_ptr()).current.height) }
    }

    /// Whether any buffer is attached (the surface is mapped).
    pub fn has_buffer(&self) -> bool {
        // SAFETY: as `current_size`.
        unsafe { sys::wlr_surface_has_buffer(self.raw.as_ptr()) }
    }
}

impl std::fmt::Debug for Surface<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface").field("id", &self.id).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests { /* scratch-surface tests modelled on toplevel.rs `ScratchToplevel` */ }
```

- [ ] **Step 5: Register the id kind, module, and exports**

In `crates/wlr/src/lib.rs`: add `mod surface;` (after `mod seat;`) and `pub use surface::{Surface, SurfaceId};` near the other id re-exports.

- [ ] **Step 6: Add the registry to `runtime.rs`**

Add the field to `RuntimeInner` (next to `toplevels`), its initialiser in **every** `RuntimeInner` literal, and the methods (copy `record_toplevel`/`forget_toplevel`/`toplevel_entry`/`clear_toplevels`):

```rust
#[derive(Clone, Copy)]
pub(crate) struct SurfaceEntry { pub(crate) raw: NonNull<sys::wlr_surface> }

// RuntimeInner:
pub(crate) surfaces: RefCell<HashMap<SurfaceId, SurfaceEntry>>,

impl Runtime {
    pub(crate) fn record_surface(&self, id: SurfaceId, raw: NonNull<sys::wlr_surface>) {
        self.inner.surfaces.borrow_mut().insert(id, SurfaceEntry { raw });
    }
    pub(crate) fn forget_surface(&self, id: SurfaceId) {
        let _ = self.inner.surfaces.borrow_mut().remove(&id);
    }
    pub(crate) fn surface_ptr(&self, id: SurfaceId) -> Option<NonNull<sys::wlr_surface>> {
        self.inner.surfaces.borrow().get(&id).map(|e| e.raw)
    }
    pub(crate) fn clear_surfaces(&self) { self.inner.surfaces.borrow_mut().clear(); }

    /// The handle for `id`, or `None` if no live surface has it.
    pub fn surface(&self, id: SurfaceId) -> Option<Surface<'_>> {
        let raw = self.surface_ptr(id)?;
        // SAFETY: an entry is removed before wlroots frees the surface; the
        // borrow is released before the handle is built.
        Some(unsafe { Surface::from_raw_with_id(raw.as_ptr(), id) })
    }
}
```

- [ ] **Step 7: Add `Event` variants and `deliver_all` arms**

`dispatch.rs` (import `SurfaceId`; add beside the toplevel variants):

```rust
SurfaceCommitted(SurfaceId),
SurfaceMapped(SurfaceId),
SurfaceUnmapped(SurfaceId),
SurfaceDestroyed(SurfaceId),
SubsurfaceCreated(SurfaceId, SurfaceId),
```

`backend.rs` `deliver_all`:

```rust
Event::SurfaceCommitted(id) => with_surface(session, id, |s| state.surface_committed(s)),
Event::SurfaceMapped(id) => with_surface(session, id, |s| state.surface_mapped(s)),
Event::SurfaceUnmapped(id) => with_surface(session, id, |s| state.surface_unmapped(s)),
Event::SurfaceDestroyed(id) => state.surface_destroyed(id),
Event::SubsurfaceCreated(p, c) => state.new_subsurface(p, c),
```

Add the five variants to the output-only `deliver` drop arm so the exhaustive `match` still compiles.

- [ ] **Step 8: Add defaulted handler methods to `ToplevelHandler`**

In `handler.rs`, beside `layer_surface_mapped`, each with an "Added additively" doc note:

```rust
fn surface_committed(&mut self, surface: &Surface<'_>) { let _ = surface; }
fn surface_mapped(&mut self, surface: &Surface<'_>) { let _ = surface; }
fn surface_unmapped(&mut self, id: SurfaceId) { let _ = id; }
fn surface_destroyed(&mut self, id: SurfaceId) { let _ = id; }
fn new_subsurface(&mut self, parent: SurfaceId, child: SurfaceId) { let _ = (parent, child); }
```

- [ ] **Step 9: Add `Bound::surface`, `link_surface`, the callbacks, and `with_surface` in `backend.rs`**

Add `surface: Option<SurfaceId>` to `Bound` (init `None` in all nine `Box::new(Bound { .. })` constructors), add `Registration::link_surface` (modelled on `link_toplevel`), and add:

```rust
fn with_surface<S>(session: &Session<'_, S>, id: SurfaceId, f: impl FnOnce(&Surface<'_>)) {
    let Some(entry) = session.runtime.surface_ptr(id) else { return };
    // SAFETY: a present entry names a live surface (removed before free); the
    // handle is scoped to `f`, which cannot drive the loop.
    let surface = unsafe { Surface::from_raw_with_id(entry.as_ptr(), id) };
    f(&surface);
}
```

Callbacks `on_surface_commit_generic`, `on_surface_map`, `on_surface_unmap`, `on_surface_destroy`, `on_new_subsurface` each: `bound_of(l)` → `session` → `(*bound).surface` (never `data`) → `dispatcher.emit(...)`. `on_surface_destroy` removes the session row and `runtime.forget_surface(id)` **before** emitting `SurfaceDestroyed`. `on_new_subsurface` reads the child pointer from the signal's payload only if verified; otherwise it emits nothing and the child is discovered when its own role announces.

- [ ] **Step 10: Install the generic listeners at every announce site**

Add `unsafe fn install_surface_listeners<S>(session, surface) -> SurfaceListeners` that: `ensure_surface_id_raw(&raw mut (*surface).addons)`; links `commit`/`map`/`unmap`/`destroy`/`new_subsurface` via `link_surface`; `record_surface`; inserts into `session.surfaces`. Call it from `on_new_toplevel`, `on_new_layer_surface`, `on_new_popup`, `on_xwayland_surface_associate` (with teardown in `on_xwayland_surface_dissociate`), and `on_session_lock_new_surface`, guarded by a `find_surface_id` check so a surface is installed once. Add `Session.surfaces: RefCell<HashMap<SurfaceId, SurfaceListeners>>` and a `SurfaceTableGuard` in `run_inner`.

- [ ] **Step 11: Add the compile-fail fixtures**

Copy `tests/ui/toplevel_escapes_handler.rs`/`.stderr` and `toplevel_from_raw_is_private.rs`/`.stderr` to the `surface_*` names and register them in `tests/compile_fail.rs`.

- [ ] **Step 12: Run the tests and the audit**

Run: `cargo test -p wlr --test surfaces && cargo test -p wlr --tests && cargo xtask coverage --check`
Expected: PASS; audit reports no new `not-yet`; `wlr_surface_has_buffer` already wrapped.

- [ ] **Step 13: Commit**

```bash
git add crates/wlr/src/{surface.rs,lib.rs,id.rs,backend.rs,dispatch.rs,runtime.rs,handler.rs} crates/wlr/tests/surfaces.rs crates/wlr/tests/ui crates/wlr/tests/compile_fail.rs
git commit -m "feat(wlr): Surface handle model and generic surface events (M9)"
```

---

## Task 2: Presentation feedback + tearing hint

The M6→M9 re-homes. Depends on `Surface`.

**Files:** Create `crates/wlr/src/presentation.rs`, `crates/wlr/src/tearing.rs`; modify `lib.rs`, `output.rs` (or `runtime.rs` for the manager accessors), `coverage/{wrapped,waived}.toml`; create `crates/wlr/tests/presentation.rs`.

**Interfaces:** Produces `PresentationFeedback` (owned handle: `create`/`send_presented`/`drop`), `PresentationEvent`, `TearingControl<'h>` + `surface_hint_from_surface`.

- [ ] **Step 1:** Wrap `wlr_presentation_event`/`_from_output` as `PresentationEvent` accessors and `wlr_presentation_feedback`/`_destroy`/`_send_presented` as an owned `PresentationFeedback` on `Surface`/`Runtime` (the manager `wlr_presentation` pointer is already stored by `create_presentation`). Wrap `wlr_presentation_surface_{sampled,scanned_out_on_output,textured_on_output}` as `Surface` methods. Wrap `wlr_tearing_control_v1` + `wlr_tearing_control_manager_v1_surface_hint_from_surface` as `TearingControl` and `Surface::tearing_hint()`.
- [ ] **Step 2:** Destroy-order tests for `PresentationFeedback` (drop then assert miss) and `TearingControl`; headless test that the tearing global is read-only until a surface exists.
- [ ] **Step 3:** Add the M9 ops to the fuzz `Operation` enum; update the ledger rows to `wrapped`.
- [ ] **Step 4:** `cargo test -p wlr --tests && cargo xtask coverage --check`; commit `feat(wlr): surface presentation feedback and tearing hint (M9)`.

---

## Task 3: Subsurface / subcompositor + compositor re-home

**Files:** Modify `crates/wlr/src/backend.rs`, `runtime.rs`, `surface.rs`, `handler.rs`, `dispatch.rs`, `coverage/*`; create `crates/wlr/tests/subsurfaces.rs`.

**Interfaces:** Produces `Subsurface<'h>` (`parent_surface_id`, `parent_state`), `Surface::subsurfaces()`; stores the subcompositor pointer currently discarded at `runtime.rs:3984`.

- [ ] **Step 1:** Store the `wlr_subcompositor` pointer in `RuntimeInner`; add `Subsurface` handle + `wlr_subsurface_try_from_wlr_surface`/`_parent_state` wrappers; expose `Surface::subsurfaces()` via `wlr_surface_for_each_surface` (Rust closure).
- [ ] **Step 2:** Move `wlr_compositor_set_renderer` to the ledger's **M13** re-home (no code); classify `wlr_subcompositor`/`wlr_subsurface` structs as wrapped once named.
- [ ] **Step 3:** Destroy-order test (drop parent, assert `Subsurface` misses); client-driven test (real client creates a subsurface, server observes `new_subsurface`).
- [ ] **Step 4:** Ledger + fuzz ops; `cargo test`; commit `feat(wlr): subsurface/subcompositor handles (M9)`.

---

## Task 4: xdg-shell remainder

**Files:** Modify `crates/wlr/src/toplevel.rs`, `popup.rs`, `decoration.rs`, `surface.rs`, `runtime.rs`, `dispatch.rs`, `handler.rs`, `coverage/*`; create `crates/wlr/tests/xdg_remainder.rs`.

**Interfaces:** Adds `Toplevel::{set_bounds,set_constrained,set_parent,set_resizing,set_suspended,set_tiled,set_wm_capabilities,wm_capabilities,state}`, the show-window-menu event, `Surface::role()` typed enum via `wlr_xdg_surface_role`, `Toplevel::from_surface`/`Popup::from_surface` (downcasts), `Positioner` (`is_complete`), decoration `configure`/`state`.

- [ ] **Step 1:** Add the `Toplevel` setters (Runtime by-id mutators mirroring `set_toplevel_size`) and `state()`/`wm_capabilities()` accessors; add the `request_show_window_menu` signal + defaulted handler method; waive `wlr_xdg_toplevel_move_event` internal.
- [ ] **Step 2:** Typed `SurfaceRole` enum; `try_from_wlr_surface` downcasts for toplevel/popup/positioner(dialog uses it too).
- [ ] **Step 3:** Positioner + decoration rows (wrap or internal per `waived.toml` notes); re-home `*_from_resource` to M13.
- [ ] **Step 4:** Client-driven commit/ack/configure test, destroy-order tests, fuzz ops, ledger; commit `feat(wlr): xdg-shell remainder (M9)`.

---

## Task 5: xdg activation token + dialog + foreign + system-bell

**Files:** Create `crates/wlr/src/xdg_activation.rs`, `xdg_dialog.rs`, `xdg_foreign.rs`, `xdg_system_bell.rs`; modify `lib.rs`, `runtime.rs`, `dispatch.rs`, `handler.rs`, `coverage/*`; create `crates/wlr/tests/xdg_protocols.rs`.

**Interfaces:** Produces `ActivationToken` handle (create/`get_name`/drop) + `Runtime::{add_activation_token,find_activation_token}`; `Dialog` (`try_from`); `xdg_foreign` registry + v1/v2 + exported/imported handles; `SystemBell` manager + ring event (defaulted handler method).

- [ ] **Step 1:** Token object handle + manager `add_token`/`find_token`/`get_name`; keep the existing `ActivationToken` snapshot type (rename the new handle to avoid collision, e.g. `ActivationTokenHandle`). Client-driven token round-trip test.
- [ ] **Step 2:** `wm_dialog_v1` manager + `Dialog` downcast; `xdg_system_bell_v1` manager + `ring` event on `ToplevelHandler`.
- [ ] **Step 3:** `xdg_foreign` v1/v2 managers, registry (`create`/`find_by_handle`), exported (`init`/`finish`), imported (+ child) handles — owned, with destroy-order tests.
- [ ] **Step 4:** fuzz ops; ledger; `cargo test`; commit `feat(wlr): xdg activation/dialog/foreign/system-bell (M9)`.

---

## Task 6: xdg toplevel icon + tag

**Files:** Create `crates/wlr/src/xdg_toplevel_icon.rs`, `xdg_toplevel_tag.rs`; modify `lib.rs`, `runtime.rs`, `dispatch.rs`, `handler.rs`, `coverage/*`; create `crates/wlr/tests/xdg_toplevel_meta.rs`.

**Interfaces:** `ToplevelIconManager` (`set_icon`/`set_sizes` events), refcounted `ToplevelIcon` owned handle (`ref`/`unref`/Drop, `buffer`); `ToplevelTagManager` (`set_tag`/`set_description` events).

- [ ] **Step 1:** Icon manager + `ToplevelIcon` refcount lifetime; destroy-order test proving `unref`/Drop balance.
- [ ] **Step 2:** Tag manager + set_tag/set_description events.
- [ ] **Step 3:** Client-driven icon/tag event tests; fuzz ops; ledger; commit `feat(wlr): xdg toplevel icon and tag (M9)`.

---

## Task 7: foreign-toplevel-management

**Files:** Create `crates/wlr/src/foreign_toplevel.rs`; modify `lib.rs`, `runtime.rs`, `dispatch.rs`, `handler.rs`, `coverage/*`; create `crates/wlr/tests/foreign_toplevel.rs`.

**Interfaces:** `ForeignToplevelManager` (`create`), owned `ForeignToplevelHandle` (`create`/`destroy`/`set_title`/`set_app_id`/`set_activated`/`set_maximized`/`set_minimized`/`set_fullscreen`/`set_parent`/`set_rectangle`/output enter-leave), and defaulted handler methods for the client requests (`activate`, `close`, `maximize`, `minimize`, `fullscreen`, `set_rectangle`).

- [ ] **Step 1:** Manager + owned handle + all `set_*`/output-enter/leave wrappers; handler methods for the request events.
- [ ] **Step 2:** Destroy-order + client-driven export test (client sees a handle the compositor created).
- [ ] **Step 3:** Fuzz ops; ledger; commit `feat(wlr): foreign-toplevel-management (M9)`.

---

## Task 8: ext-foreign-toplevel-list + ext-workspace

**Files:** Create `crates/wlr/src/ext_foreign_toplevel.rs`, `ext_workspace.rs`; modify `lib.rs`, `runtime.rs`, `dispatch.rs`, `handler.rs`, `coverage/*`; create `crates/wlr/tests/ext_protocols.rs`.

**Interfaces:** `ExtForeignToplevelList` manager + owned handle (`state`/`update_state`; `from_resource` → M13); `ExtWorkspaceManager` + owned `Workspace`/`WorkspaceGroup` handles with `set_{active,coordinates,group,hidden,name,urgent}` and commit/request events.

- [ ] **Step 1:** ext-foreign-toplevel-list manager + handle (no `from_resource`).
- [ ] **Step 2:** ext-workspace manager + workspace/group handles + commit/request events.
- [ ] **Step 3:** Client-driven workspace commit test; destroy-order; fuzz ops; ledger; commit `feat(wlr): ext foreign-toplevel list and ext workspace (M9)`.

---

## Task 9: security-context + fixes + session-lock completion

**Files:** Create `crates/wlr/src/security_context.rs`, `fixes.rs`; modify `lib.rs`, `runtime.rs`, `dispatch.rs`, `handler.rs`, `coverage/*`; create `crates/wlr/tests/security_context.rs`.

**Interfaces:** `SecurityContextManager` (`create`/`lookup_client`), `SecurityContext` (commit event, `state`); `Fixes` (`create`); `LockSurface<'h>` (`output`/`configured_size`, naming `wlr_session_lock_surface_v1_try_from_wlr_surface` + `_state`).

- [ ] **Step 1:** security-context manager + commit event + state; `fixes` create.
- [ ] **Step 2:** `LockSurface` handle exposing output + configured size; wrap the two remaining session-lock rows.
- [ ] **Step 3:** Client-driven security-context commit test; destroy-order; fuzz ops; ledger; commit `feat(wlr): security-context, fixes, lock surface (M9)`.

---

## Task 10: Ledger sweep, docs, release

**Files:** Modify `crates/wlr/coverage/{wrapped,waived}.toml`, `crates/wlr/README.md`, `crates/wlr/Cargo.toml`, `Cargo.lock` (if tracked); regenerate `crates/wlr-sys/prebuilt/bindings-docsrs.rs` only if needed (it should not be).

- [ ] **Step 1:** Apply every re-home from the Global Constraints table; reclassify each remaining M9 row as `wrapped` or `internal` with a one-line rationale; confirm zero `not-yet` M9 remain.
- [ ] **Step 2:** Run the full gate: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo xtask coverage --check`, `cargo test -p wlr --all-features --test coverage_audit`, and the client-driven/fuzz/bench legs.
- [ ] **Step 3:** Bump `crates/wlr/Cargo.toml` to **0.20.36**; add the README changelog entry listing the wrapped families.
- [ ] **Step 4:** Commit `release(wlr): 0.20.36 — M9 shell completion`.

---

## Self-Review

- **Spec coverage:** §1.1 → Task 1; §1.2 → Task 1 Steps 7–9 (via `ToplevelHandler`, ruled); §1.3 → Task 2; §1.4 → Tasks 3–9; §1.5 → Task 4; §1.6 → Task 10 (+ re-homes); §1.7 sequencing → task order; six-leg tests → per-task.
- **Placeholder scan:** Task 1 carries full code; Tasks 2–9 specify exact files, types, methods, and the wlroots symbol/behavior each wraps, following one established pattern; Task 10 enumerates the ledger policy. No `TBD`/`TODO`.
- **Type consistency:** `SurfaceId`, `Surface<'h>`, `Runtime::surface`, `install_surface_listeners`, `with_surface`, and the five handler method names are defined once in Task 1 and reused verbatim.
- **Known deviations:** the `SurfaceHandler` → `ToplevelHandler` method ruling (Global Constraints); `*_from_resource` deferred to M13; ext-image-capture-source deferred to M10.
