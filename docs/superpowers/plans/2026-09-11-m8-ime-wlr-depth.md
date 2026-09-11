# M8 IME depth (wlr) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wrap the 6 deferred IME symbols as snapshot types, session-type the relay's outgoing send order with tokens, and add the reposition event + size accessor — shipping as wlr 0.20.32 after icedtea proves it.

**Architecture:** New types in `crates/wlr/src/runtime.rs` beside the entry tables; handlers in `crates/wlr/src/backend.rs` rewritten to thread tokens with byte-identical wire behavior; one id-only event in the A12 shape. No new traits, no supertrait changes.

**Tech Stack:** Rust (MSRV 1.88), wlroots 0.20 C API via `wlr-sys` bindgen bindings.

**Spec:** `../specs/2026-08-18-wlr-100-coverage-roadmap-design.md` (§M8, §3) and icedtea `docs/superpowers/specs/2026-09-11-m8-ime-depth-design.md` (§2–§5, §8). Companion consumer plan: icedtea `docs/superpowers/plans/2026-09-11-m8-ime-consumer.md` (proves this plan's API end-to-end before publish).

## Global Constraints

- MSRV is 1.88 (`rust-version` in the workspace manifest).
- Within wlroots minor 0.20 the hand-written API is frozen: purely additive (defaulted methods, new types, new `Event` variant on a `pub(crate)` enum is internal). No new trait, no `Handlers` supertrait change (ruling A12-SEMVER).
- Per-object lifecycle signals emit NULL `data`: recover identity from `Bound`/listener-address (ruling FIX-3). Manager creation signals carry the object in `data`.
- `unsafe` blocks carry `SAFETY:` comments proving C-side invariants; no borrow held across FFI emit (copy out, drop `Ref`, then call).
- R1: no publish until the companion consumer plan's e2e is green. The publish step is a consent stop.
- Gates per task: `cargo test -p wlr`, `cargo test -p wlr-sys`, `cargo clippy --all-targets -- -D warnings`, `RUSTDOCFLAGS="-D warnings" cargo doc -p wlr --no-deps`, `cargo fmt --all --check`, `cargo test -p wlr --test coverage_audit`.
- TDD with bite-sized commits; one SDD task per commit; each task gets a scoped review before the next begins.

---

## File structure

- `crates/wlr/src/runtime.rs` — owns: `PendingImeState`, `CommittedImeState`, `PendingTextInputState`, `CommittedTextInputState` (all `pub`, `pub(crate)` constructors); `ImeActivation`, `EnteredTextInput`, `CommitSerial` token types with their send methods; readers `pending_ime_state`, `committed_ime_state`, `pending_text_input_state`, `committed_text_input_state`, `try_input_popup_surface`, `destroy_keyboard_grab`, `input_popup_size`.
- `crates/wlr/src/backend.rs` — owns: handler rewrites (`on_text_input_enable`, `on_text_input_commit`, `on_text_input_disable`, `on_input_method_commit`, destroy handlers) threading tokens; reposition emit at the commit site.
- `crates/wlr/src/dispatch.rs` — owns: `Event::InputMethodPopupRepositioned(InputPopupSurfaceId)` variant + `deliver_all` arm beside `SessionLockChanged` + `run`-path unreachable arm.
- `crates/wlr/src/handler.rs` — owns: defaulted `fn popup_repositioned(&mut self, popup: InputPopupSurfaceId)` on `SeatHandler` with the semver-rationale doc (f2cc8a9 precedent).
- `crates/wlr/tests/input_method.rs` — owns: in-crate negative tests that need no client (dangling ids, no-IME readers).
- `crates/wlr/coverage/waived.toml` + `wrapped.toml` — owns: the 6 deferred rows waived→wrapped with `item` pointing at the new types.
- `crates/wlr/Cargo.toml` (`0.20.31` → `0.20.32`) + `crates/wlr/README.md` (changelog) — owns: release mechanics (Task M8-4 only).

---

### Task M8-1: Snapshot types + readers + downcast + grab destroy

**Files:**
- Modify: `crates/wlr/src/runtime.rs` (new types + readers, beside `InputMethodEntry` ~line 149 and the popup accessors ~line 5377)
- Modify: `crates/wlr/tests/input_method.rs` (negative tests)
- Modify: `crates/wlr/coverage/waived.toml` (delete the 6 rows), `wrapped.toml` (add rows)

**Interfaces:**
- Consumes: `InputMethodEntry`/`TextInputEntry` tables (`RuntimeInner::input_method`, `text_inputs`); `InputPopupSurfaceId::dangling_nth_for_test` (existing, for negative tests)
- Produces: `PendingImeState`, `CommittedImeState`, `PendingTextInputState`, `CommittedTextInputState`; `Runtime::{pending_ime_state, committed_ime_state, pending_text_input_state, committed_text_input_state, try_input_popup_surface, destroy_keyboard_grab}` (consumed by M8-2 handlers, M8-5 consumer wiring, and overlay code)

- [ ] **Step 1: Failing negative tests**

Append to `crates/wlr/tests/input_method.rs`:

```rust
#[test]
fn dangling_ids_and_no_ime_read_empty_snapshots() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let bogus = wlr::InputPopupSurfaceId::dangling_nth_for_test(0);
    assert!(runtime.pending_ime_state().is_none());
    assert!(runtime.committed_ime_state().is_none());
    assert!(runtime.pending_text_input_state().is_none());
    assert!(runtime.committed_text_input_state().is_none());
    assert!(runtime.try_input_popup_surface(std::ptr::null_mut()).is_none());
    assert!(!runtime.destroy_keyboard_grab());
}
```

- [ ] **Step 2: Run, watch fail** — `cargo test -p wlr --test input_method dangling_ids` → FAIL (methods do not exist). Note: `try_input_popup_surface(null)` must return `None`, never dereference — the implementation null-checks first.
- [ ] **Step 3: Minimal implementation** — the four structs (owned `String`/`Option`/`Vec` fields; `pub(crate)` constructors reading `pending`/`current` C structs with null-guarded string copies), the six readers (table miss → `None`), `try_input_popup_surface` (null → `None`; else scan entries' `raw.surface` — the reverse lookup the downcast enables), `destroy_keyboard_grab` (take the registration + clear the field → `true`; absent → `false`).
- [ ] **Step 4: Run** — new test PASS; full `cargo test -p wlr` green (no behavior change yet).
- [ ] **Step 5: Coverage moves + gates + commit** — move the 6 rows waived→wrapped (`item` = the new type/method); run all six gates; commit `feat(wlr): IME/text-input snapshot reads + popup downcast + grab destroy (M8)`. Behavioral proof of the readers is the consumer plan's e2e (R1 ordering — stated, not skipped).

### Task M8-2: Token types + handler rewrite (byte-identical wire behavior)

**Files:**
- Modify: `crates/wlr/src/runtime.rs` (`ImeActivation`, `EnteredTextInput`, `CommitSerial` + their send methods)
- Modify: `crates/wlr/src/backend.rs` (handlers call tokens instead of raw `sys::` sends)

**Interfaces:**
- Consumes: snapshot readers (M8-1, for building token payloads); existing handler sites
- Produces: token APIs (consumed by M8-3 emit code and the consumer plan only indirectly — tokens are crate-internal; the observable contract is unchanged wire behavior)

- [ ] **Step 1: Define the token skeleton (compile-first)**

```rust
#[derive(Debug, Clone, Copy)]
pub(crate) struct ImeActivation {
    runtime: Runtime,
    ime: usize, // key into input_method slot (single IME: unit key)
}

impl ImeActivation {
    pub(crate) fn send_surrounding(&self, text: &str, cursor: u32, anchor: u32);
    pub(crate) fn send_content_type(&self, hint: u32, purpose: u32);
    pub(crate) fn send_change_cause(&self, cause: u32);
    pub(crate) fn finish(self); // the ONLY path to send_done
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EnteredTextInput {
    runtime: Runtime,
    ti: usize, // key into text_inputs
}

impl EnteredTextInput {
    pub(crate) fn send_preedit(&self, text: &str, cursor_begin: i32, cursor_end: i32);
    pub(crate) fn send_commit(&self, text: &str);
    pub(crate) fn send_delete(&self, before: u32, after: u32);
    pub(crate) fn finish(self); // the ONLY path to text-input send_done
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitSerial(pub u32);
```

Keep tokens `pub(crate)` in this slice: the consumer plan needs only snapshots + events; public token exposure is a later decision, not this task. Each method resolves its object through the tables and no-ops on stale (preserving every guard); each carries a `SAFETY` comment on its FFI call.

- [ ] **Step 2: Rewrite handlers to thread tokens** — enable builds `ImeActivation` then calls its sends then `finish`; commit reuses the live activation; disable/destroy consume the token via `deactivate(tok)`; enter/commit paths mirror app-side. Delete the now-dead inline `sys::send_*` call sites as each handler converts (so at no point do two paths emit the same event).
- [ ] **Step 3: Run the relay suite unchanged** — `cargo test -p wlr --test input_method` (all existing tests, unmodified) + full `cargo test -p wlr`. Expected: PASS with zero test edits — the byte-identical-wire-behavior proof.
- [ ] **Step 4: Grep audit** — `grep -rn "wlr_input_method_v2_send_\|wlr_text_input_v3_send_" crates/wlr/src/` must show hits ONLY inside the token `impl` blocks. Anything else is a missed call site — fix before proceeding.
- [ ] **Step 5: Gates + commit** — all six gates; commit `feat(wlr): session-typed IME relay sends (M8)`.

### Task M8-3: Reposition event + size accessor

**Files:**
- Modify: `crates/wlr/src/dispatch.rs`, `handler.rs`, `runtime.rs` (`input_popup_size`), `backend.rs` (emit at the text-input commit site)
- Modify: `crates/wlr/tests/input_method_popup.rs` (lifecycle-neutrality still holds with the new arm present)

**Interfaces:**
- Consumes: tokenized commit path (M8-2 — emit AFTER the re-forward, before or after `finish`? Decide: after `finish`, so observers see a settled relay); `input_method_popups` table liveness check
- Produces: `Event::InputMethodPopupRepositioned`, `SeatHandler::popup_repositioned` (defaulted), `Runtime::input_popup_size(popup) -> Option<(i32, i32)>` (consumed by consumer Task M8-5)

- [ ] **Step 1: Failing compile assertion** — extend `input_method_popup.rs` with a handler overriding `popup_repositioned` (proves the method exists, is defaulted, and takes `InputPopupSurfaceId`).
- [ ] **Step 2: Run, watch fail** — undefined method.
- [ ] **Step 3: Minimal implementation** — variant + `deliver_all` arm beside `SessionLockChanged` + `run`-path unreachable arm (run never registers an IME manager — same comment shape as the Created/Destroyed arms); defaulted trait method with the f2cc8a9 semver doc; emit site: in the commit handler, after the token `finish`, `if !input_method_popups.borrow().is_empty() { emit Repositioned }` per tracked popup (borrow released before emit); size accessor reading the attached buffer size or `None`.
- [ ] **Step 4: Run** — new compile assertion PASS; full suite green; `input_method_popup` lifecycle test still asserts no synthesis.
- [ ] **Step 5: Gates + commit** — all six gates; commit `feat(wlr): popup reposition event + size accessor (M8)`.

### Task M8-4: Release 0.20.32 (freeze; publish is a consent stop)

**Files:**
- Modify: `crates/wlr/Cargo.toml` (`0.20.31` → `0.20.32` per `docs/RELEASING.md` patch checklist), `crates/wlr/README.md` (0.20.32 changelog: snapshots, tokens, reposition event — in the tone of the 0.20.31 entry), coverage ledgers (final moves)

**Interfaces:**
- Consumes: green consumer-plan e2e (R1 gate — do NOT start this task before it)
- Produces: published wlr 0.20.32 (consumed by consumer Task M8-8)

- [ ] **Step 1: Coverage audit** — `cargo test -p wlr --test coverage_audit` PASS (every wrapped symbol referenced; no orphan rows).
- [ ] **Step 2: Version + changelog** — per above.
- [ ] **Step 3: All six gates green** on the freeze commit (exact command in Global Constraints).
- [ ] **Step 4: Commit** — `git commit -m "release(wlr): 0.20.32 — IME/text-input depth (M8)"`.
- [ ] **Step 5: Push + PR into `develop`; CI green.** Then STOP — `cargo publish` requires explicit owner approval (same consent stop as 0.20.30/0.20.31). On approval: `cargo package -p wlr` (watch for `Compiling wlr-sys` without a path), `cargo publish -p wlr`; `wlr-sys` unchanged → no sys publish.
