# wlr 100% Coverage Roadmap — Design

**Date:** 2026-08-18
**Status:** Approved design; supersedes nothing — extends the 2026-08-03 safe-wrapper design.
**Scope:** The path from the current `wlr` crate state (post-M4.2, release 0.20.18) to
100% coverage of the wlroots 0.20 public API in the safe `wlr` crate, with
`wlr-sys` verified complete along the way.

## Goals and decisions

Four decisions were made during brainstorming and are fixed:

1. **Coverage bar: completionist.** Every wlroots 0.20 public API gets a safe,
   idiomatic surface. Raw `as_ptr()`/`from_ptr` escape hatches remain, but a
   symbol reachable only through a raw pointer does not count as covered.
2. **Version scope: 0.20 first.** `develop` (wlroots 0.20) is driven to 100%.
   Backports to `support/wlroots-0.19/0.17/0.15` are decided per-subsystem
   afterwards, cherry-picked and re-verified per line as usual. They are out of
   scope for this design.
3. **Tracking: CI-enforced audit.** Coverage is measured mechanically against
   the generated `bindings.rs`, not a hand-maintained checklist. CI fails on
   any symbol that is neither wrapped nor explicitly waived.
4. **Execution: sequential themed milestones.** The M-series continues:
   M5–M13, one feature branch each, ordered so foundations land before their
   dependents.

`wlr-sys` is already near-100% by construction — the build-time header scan
binds every installed public header. Its residual risk is blocklist/interop
drift, which the audit and the existing `tests/interop.rs` cover; no new
binding work is planned there.

## The coverage audit (M5.0 — built before any new wrappers)

A test/xtask in the `wlr` crate, `coverage_audit`:

1. **Ground truth:** locate the freshest generated `bindings.rs` (the
   mtime-aware `find` documented in CLAUDE.md) and extract every public
   `wlr_*` function and type. Because ground truth is regenerated per build, a
   wlroots patch release that adds symbols breaks CI automatically.
2. **Ledger:** diff against two committed files in `crates/wlr/coverage/`:
   - `wrapped.toml` — symbols the safe API covers, each mapped to the wrapping
     module/type. Entries are validated against the actual source (a
     test-time scan of `sys::` uses) so they cannot go stale silently.
   - `waived.toml` — symbols deliberately not wrapped, each with a reason
     category (`interface-impl-only`, `deprecated`, `internal`,
     `superseded-by`, `foreign-library`) and, for `not-yet` entries, a
     milestone tag.
3. **Gate:** CI fails if any symbol is in neither file. The `not-yet` entries
   **are** the backlog; **100% coverage is defined as `waived.toml` containing
   zero `not-yet` entries.**

`cargo xtask coverage` prints a per-header percentage table for burn-down
visibility. During M5.0 setup, `waived.toml` is pre-populated with every
currently-unwrapped symbol tagged by milestone, so the burn-down is visible
from day one.

The audit runs in CI across the existing feature matrix; "100%" means 100% in
the all-features configuration.

## Milestone map

Each milestone is one feature branch off `develop`, following the established
M-series process. Sizes are approximate header counts.

| # | Milestone | Contents | Ordering rationale |
|---|---|---|---|
| **M5** | Render & scene foundations | `render/*` (renderer, texture, pass, allocator, swapchain, dmabuf, drm_format_set, drm_syncobj; egl/gles2/vulkan/pixman feature-gated), deep `wlr_scene`, `damage_ring`, util (`box`, `region`, `transform`, `edges`, `log`, `addon`) | Nearly everything downstream hands out buffers, textures, or scene nodes. Unblocks the most. |
| **M6** | Output stack completion | `output_layout`, `output_management_v1`, `output_power_management_v1`, `xdg_output_v1`, `gamma_control_v1`, `output_layer`, `output_swapchain_manager`, `fractional_scale_v1`, `viewporter`, `presentation_time`, `tearing_control_v1` | Builds on M5 swapchain/renderer types; `output.rs` is the base. |
| **M7** | Input long tail I: pointer & cursor | `cursor`, `xcursor_manager`, `cursor_shape_v1`, `pointer` depth, `pointer_constraints_v1`, `relative_pointer_v1`, `pointer_gestures_v1`, `touch`, `switch` | Seat/keyboard base exists; cursor is the largest missing input piece. |
| **M8** | Input long tail II: keyboard, tablet, IME | `keyboard` depth, `keyboard_group`, `keyboard_shortcuts_inhibit`, `tablet_tool`/`tablet_pad`/`tablet_v2`, `virtual_keyboard_v1`, `virtual_pointer_v1`, `transient_seat_v1`, `input_method_v2`, `text_input_v3` | IME/text-input is the hardest state machine in wlroots; comes after simpler input matures the patterns. |
| **M9** | Shell completion | `xdg_shell` remainder (popups, positioner), `xdg_activation_v1`, `xdg_dialog_v1`, `xdg_foreign` v1/v2/registry, `xdg_system_bell_v1`, `xdg_toplevel_icon_v1`, `xdg_toplevel_tag_v1`, `foreign_toplevel_management_v1`, `ext_foreign_toplevel_list_v1`, `ext_workspace_v1`, `session_lock_v1`, `security_context_v1`, `compositor`/`subcompositor` depth, `fixes` | Popup trees need M5 scene; `toplevel.rs`/`layer.rs` are the base. |
| **M10** | Buffers, screencapture & DRM | `linux_dmabuf_v1`, `linux_drm_syncobj_v1`, `shm`, `single_pixel_buffer_v1`, `drm` (types), `drm_lease_v1`, `screencopy_v1`, `export_dmabuf_v1`, `ext_image_capture_source_v1`, `ext_image_copy_capture_v1` | Pure consumers of M5 buffer/texture types. |
| **M11** | Selection & misc protocols | `data_control_v1`, `ext_data_control_v1`, `primary_selection` (+`_v1`), `idle_inhibit_v1`, `idle_notify_v1`, `alpha_modifier_v1`, `content_type_v1`, `server_decoration` remainder | Small, independent, low-risk. |
| **M12** | Color & xwayland | `color_management_v1`, `color_representation_v1`, `render/color.h`; `xwayland/*` (feature-gated) | Color needs M5's pipeline types; xwayland is gated and self-contained. |
| **M13** | Interfaces & custom impls | `interfaces/*` (custom backends, outputs, input devices), `backend/interface.h`, `render/interface.h` | The inverse FFI direction — Rust implementing wlroots vtables. Hardest safety design; done last with all patterns mature. |

M5 is the long pole and may split into M5.1 (render types) / M5.2 (scene) if
it drags. M5.0 (the audit) precedes all wrapper work.

## Wrapper conventions for the hard categories

The established patterns — borrow-scoped handles, destroy-listener UAF guards,
observer-based events — remain the default. Five categories get explicit
rules:

1. **Feature-gated subsystems** (vulkan, gles2, drm-backend, libinput,
   xwayland, color-management): `wlr` re-exports `wlr-sys` feature names 1:1 —
   no renaming layer. Gated modules are `cfg`'d on the existing `wlr_has_*`
   mechanism; the audit runs per feature configuration.
2. **Vtable/interface impls (M13):** a trait per interface plus a `Box`-pinned
   shim struct owning the C vtable and a pointer back to the trait object. The
   pattern is written once for `wlr_buffer_impl` (the smallest interface),
   reviewed hard, then replicated. No inheritance emulation.
3. **Protocol state machines** (input-method, text-input, session-lock,
   output-management): C-side sequencing rules (commit/ack cycles, serial
   matching) are encoded in the type system — e.g.
   `OutputConfigurationPending → test()/apply() → consumed` — rather than
   free methods callable out of order. These milestones are sized generously
   for this reason.
4. **Escape hatches:** `as_ptr()`/`from_ptr` stay on every handle as interop
   tools, but the audit does not count a symbol as wrapped because a raw
   pointer can reach it.
5. **Cross-crate types** (`xkb_*`, `libinput_*`, pixman): the safe crate wraps
   *wlroots'* use of them but never re-wraps the foreign library itself.
   Audit category: `foreign-library`.

## Testing standard (six legs, per milestone)

1. **Headless integration tests** — the established style: bring up the
   headless backend, exercise the wrapper, assert via scene-node/event
   observation. Every new subsystem gets at least one.
2. **Client-driven protocol tests** for the state machines (M6, M8, M9): a
   real Wayland client thread using `wayland-client` (which sits on
   `wayland-sys`, preserving type identity) drives the protocol from the
   client side — the only honest way to reach commit/ack sequencing.
3. **Destroy-order/UAF tests** — every handle-owning wrapper gets a test that
   destroys the C object first and asserts the wrapper observes it. Miri
   cannot cross FFI; these tests are the memory-safety evidence.
4. **Coverage audit** in CI across the feature matrix, plus the existing
   docs.rs snapshot regeneration check.
5. **Fuzzing** — `cargo-fuzz` (libFuzzer + ASan) targets in a `fuzz/`
   workspace member, run as a scheduled nightly CI job (nightly toolchain,
   kept out of the MSRV lane):
   - *Operation-sequence fuzzing* of stateful wrappers: an
     `arbitrary`-derived enum of operations (create/configure/commit/ack/
     destroy in any order, wrong serials, double-destroys) replayed against
     the state-machine wrappers on a headless backend, asserting no panic, no
     UAF (ASan is the oracle), and that illegal sequences are rejected.
   - *Targeted fuzzing* of the hand-written substrate: `wl_list_iter` /
     `container_of!` / signal dispatch under adversarial list mutation from
     inside handlers, and `wlr_scene` reparent/destroy storms.
   - Each milestone that adds a state machine adds its operations to the fuzz
     enum before the milestone merges; the corpus is cumulative.
6. **Benchmarks** — `criterion` benches in `crates/wlr/benches/`, measuring
   the safe layer's *overhead*, not wlroots itself: each bench pairs a
   wrapper call against the equivalent raw `sys` sequence (handle
   borrow/upgrade, event dispatch through the observer layer vs a bare
   `wl_listener`, scene-node ops). CI runs them on `develop` pushes and
   records results as artifacts for trend-watching — informational, not a
   gate (shared-runner noise makes hard thresholds flaky). Headline number to
   protect: per-event dispatch overhead stays in the tens-of-nanoseconds
   class.

## Release cadence

One `wlr` patch release per completed milestone (additive API within 0.20.x is
semver-clean), following `docs/RELEASING.md`. M5 likely lands as two releases.
`wlr-sys` is released only if an audit or blocklist finding requires it.
Roughly 9–11 releases to 100%, with `cargo xtask coverage` showing the
burn-down throughout.

## Out of scope

- Backports to the `support/*` lines (decided per-subsystem after 0.20 reaches
  100%).
- Wrapping foreign libraries (xkbcommon, libinput, pixman) beyond wlroots'
  own use of their types.
- Any change to the frozen `wlr-sys` hand-written API within the 0.20 line.
