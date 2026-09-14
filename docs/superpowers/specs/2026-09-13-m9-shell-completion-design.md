# wlr M9 — Shell Completion Design

Status: proposed · Date: 2026-09-13 · Milestone: M9 (shell completion)
Roadmap: `docs/superpowers/specs/2026-08-18-wlr-100-coverage-roadmap-design.md`

## Goal

Reach zero `not-yet` rows tagged `milestone = "M9"` in
`crates/wlr/coverage/waived.toml`, by wrapping the shell-completion surface of
wlroots 0.20 behind the crate's existing borrow-scoped-handle conventions, and
prove it with the roadmap's six-leg testing standard.

M9 is delivered as **two PRs**, in order:

- **PR-0 — test-infrastructure standup.** Build the three test legs that do not
  exist yet (client-driven, fuzzing, benchmarks) and de-duplicate the headless
  harness. Repo-wide, not M9-specific; lands first so M9 can meet the bar.
- **PR-1 — M9.** A single large PR wrapping the Surface handle model and every
  remaining shell-completion family, staged as ordered commits.

## Non-goals

- No change to the frozen `wlr-sys` hand-written API within the 0.20 line.
- No backports to `support/*`.
- No `*_from_resource` wrappers (re-homed to M13; see Rulings).
- No renderer-ownership redesign (M13).
- Additive API only within 0.20.x; no `Handlers` supertrait change.

## Where M9 stands today

Reconnaissance against `crates/wlr/src` and
`crates/wlr/coverage/waived.toml`:

**Already wrapped (out of the backlog):** xdg-activation *manager* +
`ActivationToken` value snapshot + `RequestActivate` event; session-lock
*manager* and full internal lifecycle (`is_session_locked`, lock-surface trees,
`send_locked`); presentation *global* + `set_scene_presentation`; tearing-control
*manager*; xdg-shell `Toplevel`/`Popup`/`LayerSurface` handles; xdg-decoration;
`create_xdg_shell`.

**Served indirectly or internal-only — ledger work, not code:**
`wlr_surface_send_enter`/`send_leave`/`set_preferred_buffer_scale`/
`set_preferred_buffer_transform` (scene does it); `wlr_surface_point_accepts_input`/
`surface_at`/`send_frame_done` (scene hit-test / frame-done);
`wlr_subcompositor`/`wlr_subsurface` (built internally, pointer discarded);
`wlr_surface_state*`, `wlr_xdg_surface_configure`,
`wlr_xdg_toplevel_requested`, `wlr_layer_surface_v1_state`,
`wlr_layer_surface_v1_get_exclusive_edge`, session-lock surface state.

**Genuinely missing:** the public `Surface` handle model; per-surface
presentation feedback; tearing hint; subsurface role; activation token API;
xdg-dialog; xdg-foreign v1/v2;
xdg-system-bell; xdg-toplevel-icon; xdg-toplevel-tag; foreign-toplevel-management;
ext-foreign-toplevel-list; ext-workspace; security-context; fixes; the xdg-shell
`set_*` remainder.

**Test legs:** headless integration (39 binaries), destroy-order/UAF, and the
coverage audit exist. Client-driven (`wayland-client`), fuzzing, and benchmarks
are **absent**.

---

## Part 0 — PR-0: Test-infrastructure standup

### 0.1 Client-driven leg

- Add `wayland-client` and `wayland-protocols` as dev-dependencies of `wlr`
  (0.31 line, matching the `wayland-sys` 0.31 identity `wlr-sys` already links).
- `crates/wlr/tests/common/client.rs`: a harness that (a) starts a headless
  compositor in the calling thread, (b) adds a socket with
  `wl_display_add_socket_auto`-equivalent and exposes `WAYLAND_DISPLAY` to a
  spawned client thread, (c) drives the client through create/commit/ack, and
  (d) lets the server-side handler assertions run after the dispatch loop
  drains.
- One seed test proves the harness end to end (xdg-toplevel create → map
  observed by the handler).
- **Open risk:** `Display` may not currently expose socket creation. If not,
  PR-0 adds an additive `Display` accessor. Confirmed during PR-0, not assumed.

### 0.2 Fuzz leg

- A standalone `fuzz/` cargo-fuzz crate, **excluded** from the root workspace so
  the stable/MSRV lanes stay green (roadmap: nightly-only, out of the MSRV lane).
- A cumulative `Operation` enum (`arbitrary`) whose variants express
  create/configure/commit/ack/destroy in any order, wrong serials, and
  double-destroys. PR-0 seeds it with the **existing** state machines
  (toplevel, popup, layer, activation, session-lock, IME, output config); each
  later milestone appends its own ops.
- One fuzz target replays operations on a headless backend and asserts no panic
  and no use-after-free, with ASan as the oracle.
- A scheduled nightly `fuzz.yml` CI job.

### 0.3 Bench leg

- `crates/wlr/benches/` + a `criterion` dev-dependency and `[[bench]]` targets.
- Three benches, each pairing the safe layer against the equivalent raw `sys`
  sequence: event dispatch through the observer layer vs a bare `wl_listener`;
  handle borrow/upgrade; a scene-node operation.
- CI runs benches on `develop` pushes and records results as artifacts
  (informational; no hard thresholds).

### 0.4 Harness de-duplication

Move the ~35 private `headless_env()` copies into `tests/common` (only 4 of 39
binaries use the shared module today) and migrate every test to it.

### 0.5 Surface

No change to `wlr`'s wrapped public API — dev-dependencies, tests, benches, fuzz
crate, and CI only.

---

## Part 1 — PR-1: M9

Staged as ordered commits on one branch (see Sequencing). Ordered so each commit
compiles, passes its own tests, and leaves the audit no worse.

### 1.1 Surface handle model (`crates/wlr/src/surface.rs`)

- `pub struct SurfaceId(pub(crate) u64)` with the usual
  `dangling_for_test`/`dangling_nth_for_test`; process-wide counter from
  `id.rs`.
- `pub struct Surface<'h> { raw: NonNull<sys::wlr_surface>, id: SurfaceId, _scope: PhantomData<&'h ()> }`
  with a private `pub(crate) unsafe fn` constructor; not `Send`/`Sync`.
- The id attaches to `wlr_surface.addons` under a **new addon kind** distinct
  from the role `ID_ADDON_IMPL`, so a surface can carry both a role id
  (`ToplevelId`/`PopupId`/`LayerSurfaceId`) and a `SurfaceId`. `addon.rs`'s
  "one id per `(owner, impl)` per set" rule is preserved.
- `RuntimeInner.surfaces: HashMap<SurfaceId, SurfaceEntry>` (raw pointer plus
  role tag); inserted when the crate first sees a surface, purged on
  `wlr_surface.events.destroy`.
- Operations: `extents`, current-size/state accessors, `effective_damage`,
  `output`, `root_surface`, `buffer_source_box`, `accepts_touch`,
  `accepts_tablet_v2`, `role` (`SurfaceRole` enum), `send_frame_done`,
  `surface_at`, `point_accepts_input`, `set_preferred_buffer_scale`,
  `set_preferred_buffer_transform`, `map`, `unmap`, `for_each_surface` (Rust
  closure instead of the C iterator callback), `synced` (a borrow-scoped
  `SyncedState` handle with `get_state`/`finish`/`init`),
  `lock_pending`/`unlock_cached`, with `reject_pending` deferred to M13 as
  `interface-impl-only` (the one bound C variadic cannot be forwarded from
  Rust). State accessors (`wlr_surface_state`,
  `state_field`, `state_has_buffer`) are added only if a consumer-facing
  accessor is warranted; otherwise those symbols are reclassified `internal`
  (see Rulings).
- Downcasts onto existing handles where the crate already holds a surface:
  `try_from_wlr_surface` for toplevel/popup/subsurface/layer/lock.

### 1.2 Surface events and handler

- A generic `wlr_surface.events.{commit,map,unmap,destroy,new_subsurface}`
  listener is installed for every surface the crate sees.
- Delivered through a new additive, defaulted `SurfaceHandler` trait
  (`surface_commit`, `surface_map`, `surface_unmap`, `surface_destroyed`,
  `new_subsurface`), following the existing null-`data` identity rule
  (`Bound`-carried id, never a `data` cast).
- Role listeners are left in place: role handlers keep their role-specific
  semantics; the generic surface handlers are additional.

### 1.3 Surface-dependent re-homes

- **Presentation per-surface feedback:** an owned `PresentationFeedback` handle
  (create / `send_presented` / destroy / Drop) plus the
  `sampled`/`scanned_out_on_output`/`textured_on_output` hint calls and the
  `presentation_event`/`event_from_output` accessors. Currently deferred at
  `runtime.rs:8571-8576` pending the surface model.
- **Tearing hint:** a `TearingControl` borrow-scoped handle plus
  `surface_hint_from_surface`. Currently deferred at `runtime.rs:7620`.

### 1.4 Protocol family modules

One module per C header, matching the existing convention:

- `xdg_activation` — token object handle: create/destroy/`get_name` plus
  `add_token`/`find_token` (client-mint path; the consumption snapshot
  `ActivationToken` already exists).
- `xdg_dialog` — `wm_dialog_v1` manager + dialog access via
  `try_from_wlr_xdg_toplevel`.
- `xdg_foreign` — registry (`create`/`find_by_handle`), v1 and v2 managers,
  exported (`init`/`finish`) and imported (+ child) handles. Stateful; owned
  handles with their own registries.
- `xdg_system_bell` — manager + ring event.
- `xdg_toplevel_icon` — icon manager (`set_icon`/`set_sizes` events) + a
  refcounted icon object exposed as an owned handle (`ref`/`unref`/Drop).
- `xdg_toplevel_tag` — manager + tag/description events.
- `foreign_toplevel` — management manager + owned `ForeignToplevelHandle`
  (create/destroy/`set_*`/output enter-leave/events).
- `ext_foreign_toplevel` — list manager + owned handle (create/destroy/
  `from_resource` re-homed to M13/state/update_state).
- `ext_workspace` — manager + owned workspace and group handles; commit/request
  events.
- `security_context` — manager + `lookup_client` + commit event + state.
- `subsurface`/`subcompositor` — `Subsurface` borrow-scoped handle
  (`try_from_wlr_surface`, `parent_state`); `RuntimeInner` starts storing the
  subcompositor pointer currently discarded at `runtime.rs:3984`.
- `fixes` — `wlr_fixes` + create.
- session-lock completion — `LockSurface` borrow-scoped handle exposing
  `output`/configured size, naming `try_from_wlr_surface` and `state`.

### 1.5 xdg-shell remainder

Fill the `Toplevel`/`Popup` gaps: `set_bounds`, `set_constrained`,
`set_parent`, `set_resizing`, `set_suspended`, `set_tiled`,
`set_wm_capabilities`, `wm_capabilities`, a `state` accessor, and the
show-window-menu event. The move-event serial is deliberately dropped
(waive-internal).

### 1.6 Ledger rulings

Definitive milestone re-homes (applied in `waived.toml`):

| Symbols | New home | Rationale |
|---|---|---|
| every `*_from_resource` (`wlr_surface_from_resource`, `wlr_xdg_surface_from_resource`, `wlr_xdg_toplevel_from_resource`, `wlr_xdg_popup_from_resource`, `wlr_xdg_positioner_from_resource`, `wlr_layer_surface_v1_from_resource`, `wlr_ext_foreign_toplevel_handle_v1_from_resource`, `wlr_region_from_resource`) | **M13** | Exist to serve custom protocol impls; the crate gets identity from callbacks, not resources. |
| `wlr_ext_foreign_toplevel_image_capture_source_manager_v1*` (4) | **M10** | Depends on ext-image-capture. |
| `wlr_surface_get_content_type_v1` | **M11** | content_type_v1 belongs to M11. |
| `wlr_compositor_set_renderer` | **M13** | Tied to renderer-ownership design, same bucket as `begin_render_pass`. |
| `wlr_surface_reject_pending` | **M13** | The one C variadic; Rust cannot forward a `va_list`, and wlroots only accepts it inside a client-commit hook. |

In-place classifications (M9 stays, reason changes from `not-yet`):

- served-indirectly rows move to `wrapped` with a `wrapped.toml` row naming the
  public API, **or** to `internal` when the crate never issues the call.
- `wlr_xdg_client`, `wlr_xdg_positioner`, `wlr_xdg_surface_configure`,
  `wlr_xdg_surface_state*`, `wlr_xdg_toplevel_configure*`,
  `wlr_xdg_toplevel_requested`, `wlr_layer_surface_v1_state*`,
  `wlr_surface_state*`: `internal` unless a consumer-facing accessor is added,
  in which case `wrapped`.
- `wlr_session_lock_surface_v1_state`/`try_from_wlr_surface`: wrapped by the new
  `LockSurface` handle.
- `wlr_xdg_surface_role`: wrapped by `SurfaceRole`.
- `presentation_time`/`tearing_control`: stay **M9** (the ledger is authoritative;
  the roadmap's M6 blurb is stale).

The milestone is complete only when `cargo xtask coverage` reports **zero**
`not-yet` rows tagged M9.

### 1.7 Sequencing (ordered commits on the M9 branch)

1. Surface core (handle, id, registry, events, accessors) + wrapped rows + tests.
2. Presentation feedback + tearing hint.
3. Subsurface/subcompositor + `compositor_set_renderer` re-home.
4. xdg-shell remainder.
5. xdg activation token + dialog + foreign + system-bell.
6. xdg toplevel icon + tag.
7. foreign-toplevel-management.
8. ext-foreign-toplevel-list + ext-workspace.
9. security-context + fixes + session-lock completion.
10. Ledger sweep to zero M9 `not-yet`; docs.rs snapshot; release 0.20.36.

---

## Testing standard (six legs)

1. **Headless integration** per subsystem (post-PR-0, shared `tests/common`).
2. **Client-driven protocol tests** (from PR-0) for the state machines:
   xdg commit/ack, activation token round trip, session-lock locked flow,
   foreign-toplevel export, icon/tag events, workspace commit, security-context
   commit.
3. **Destroy-order/UAF** per new owned handle (foreign-toplevel, workspace,
   icon, presentation feedback, activation token, subsurface, lock surface).
4. **Coverage audit** across the feature matrix + docs.rs snapshot regeneration.
5. **Fuzzing** — M9 operations appended to the cumulative `Operation` enum
   before merge.
6. **Benchmarks** — a `Surface` handle dispatch bench added.

## Risks and open questions

- `Display` socket creation for client tests may need an additive accessor
  (resolved in PR-0).
- `wayland-client`/`wayland-protocols` must not raise the MSRV lane above 1.88
  (checked in PR-0).
- Adding a generic `wlr_surface.events.commit` listener alongside role listeners
  risks double handling; the design keeps them as distinct handler methods and
  a test asserts exactly one delivery per commit.
- `wlr_toplevel_icon_v1` refcount lifetime and `xdg_foreign` imported-handle
  lifetime are the two hardest ownership models; both get destroy-order tests
  before their commits are accepted.
- The single-PR M9 branch will be large; whole-branch `/lex-review` runs before
  merge, and each ordered commit is individually reviewable.

## Release

PR-0 makes no release. PR-1 lands as one patch release, **0.20.36**, following
`docs/RELEASING.md`.
